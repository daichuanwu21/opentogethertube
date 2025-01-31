use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::Future;
use http_body_util::{BodyExt, Full};
use hyper::service::Service;
use hyper::StatusCode;
use hyper::{body::Incoming as IncomingBody, Request, Response};
use once_cell::sync::Lazy;
use ott_balancer_protocol::monolith::{RoomMetadata, Visibility};
use ott_balancer_protocol::RoomName;
use reqwest::Url;
use route_recognizer::Router;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::balancer::{BalancerContext, BalancerLink};
use crate::client::client_entry;
use crate::monolith::{monolith_entry, BalancerMonolith};

static NOTFOUND: &[u8] = b"Not Found";

static ROUTER: Lazy<Router<&'static str>> = Lazy::new(|| {
    let mut router = Router::new();
    router.add("/api/status", "health");
    router.add("/api/balancing", "status");
    router.add("/api/status/metrics", "metrics");
    router.add("/api/room/:room_name", "room");
    router.add("/monolith", "monolith");
    router.add("/", "other");
    router.add("*", "other");
    router
});

/// A service that handles HTTP requests.
///
/// An instance of this service is spawned for each incoming connection.
#[derive(Clone)]
pub struct BalancerService {
    pub(crate) ctx: Arc<RwLock<BalancerContext>>,
    pub(crate) link: BalancerLink,
    pub(crate) addr: std::net::SocketAddr,
}

#[async_trait::async_trait]
impl Service<Request<IncomingBody>> for BalancerService {
    type Response = Response<Full<Bytes>>;
    type Error = hyper::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, req: Request<hyper::body::Incoming>) -> Self::Future {
        fn mk_response(s: String) -> anyhow::Result<Response<Full<Bytes>>, hyper::Error> {
            Ok(Response::builder().body(Full::new(Bytes::from(s))).unwrap())
        }

        let ctx: Arc<RwLock<BalancerContext>> = self.ctx.clone();
        let link = self.link.clone();
        let addr = self.addr;

        let Ok(route) = ROUTER.recognize(req.uri().path()) else {
            warn!("no route found for {}", req.uri().path());
            return Box::pin(async move {Ok(not_found())});
        };
        info!(
            "Inbound request: {} {} => {}",
            req.method(),
            req.uri().path(),
            route.handler()
        );

        Box::pin(async move {
            let res = match **route.handler() {
                "health" => mk_response("OK".to_owned()),
                "status" => {
                    let ctx_read = ctx.read().await;
                    let rendered = [
                        format!("monoliths: {}", ctx_read.monoliths.len()),
                        format!("mappings: {:#?}", ctx_read.rooms_to_monoliths),
                    ]
                    .join("\n");
                    mk_response(rendered)
                }
                "metrics" => mk_response("TODO: prometheus metrics".to_owned()),
                "room" => {
                    let Some(room_name) = route.params().find("room_name") else {
                        return Ok(not_found());
                    };

                    let room_name: RoomName = room_name.to_owned().into();
                    if crate::websocket::is_websocket_upgrade(&req) {
                        debug!("upgrading to websocket");
                        let (response, websocket) = crate::websocket::upgrade(req, None).unwrap();

                        // Spawn a task to handle the websocket connection.
                        let _ = tokio::task::Builder::new().name("client connection").spawn(
                            async move {
                                if let Err(e) = client_entry(room_name, websocket, link).await {
                                    error!("Error in websocket connection: {}", e);
                                }
                            },
                        );

                        // Return the response so the spawned future can continue.
                        Ok(response)
                    } else if room_name.to_string() == "list" {
                        // special case for listing rooms
                        match list_rooms(ctx.clone()).await {
                            Ok(res) => Ok(res),
                            Err(e) => {
                                error!("error listing rooms: {}", e);
                                mk_response("error listing rooms".to_owned())
                            }
                        }
                    } else {
                        let ctx_read = ctx.read().await;
                        let monolith = if let Some(monolith_id) =
                            ctx_read.rooms_to_monoliths.get(&room_name)
                        {
                            info!("found room {} in monolith {}", room_name, monolith_id);
                            ctx_read.monoliths.get(monolith_id)
                        } else {
                            ctx_read.select_monolith().ok()
                        };
                        if let Some(monolith) = monolith {
                            info!("proxying request to monolith {}", monolith.id());
                            if let Ok(res) = proxy_request(req, monolith).await {
                                Ok(res)
                            } else {
                                mk_response("error proxying request".to_owned())
                            }
                        } else {
                            mk_response("no monoliths available".to_owned())
                        }
                    }
                }
                "monolith" => {
                    if crate::websocket::is_websocket_upgrade(&req) {
                        let (response, websocket) = match crate::websocket::upgrade(req, None) {
                            Ok((response, websocket)) => (response, websocket),
                            Err(e) => {
                                error!("error upgrading websocket for monolith: {}", e);
                                return Ok(not_found());
                            }
                        };

                        // Spawn a task to handle the websocket connection.
                        let _ = tokio::task::Builder::new()
                            .name("monolith connection")
                            .spawn(async move {
                                if let Err(e) = monolith_entry(addr, websocket, link).await {
                                    error!("Error in websocket connection: {}", e);
                                }
                            });

                        // Return the response so the spawned future can continue.
                        Ok(response)
                    } else {
                        mk_response("must upgrade to websocket".to_owned())
                    }
                }
                "other" => {
                    let ctx_read = ctx.read().await;
                    let monolith = ctx_read.random_monolith().ok();
                    if let Some(monolith) = monolith {
                        info!("proxying request to monolith {}", monolith.id());
                        if let Ok(res) = proxy_request(req, monolith).await {
                            Ok(res)
                        } else {
                            mk_response("error proxying request".to_owned())
                        }
                    } else {
                        mk_response("no monoliths available".to_owned())
                    }
                }
                _ => Ok(not_found()),
            };
            res
        })
    }
}

/// HTTP status code 404
fn not_found() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Full::new(NOTFOUND.into()))
        .unwrap()
}

async fn proxy_request(
    in_req: Request<IncomingBody>,
    target: &BalancerMonolith,
) -> anyhow::Result<Response<Full<Bytes>>> {
    let client = target.http_client();
    let mut url: Url =
        format!("http://{}{}", target.proxy_address(), in_req.uri().path()).parse()?;
    url.set_query(in_req.uri().query());
    let method = in_req.method().clone();
    let headers = in_req.headers().clone();
    // TODO: update X-Forwarded-For header
    // TODO: stream the body instead of loading it all into memory?
    let body: Bytes = in_req.collect().await?.to_bytes();
    let out_body = reqwest::Body::from(body);
    let req = client
        .request(method, url)
        .headers(headers)
        .body(out_body)
        .build()?;
    let res = client.execute(req).await?;
    let status = res.status();
    let mut builder = Response::builder().status(status);
    for (k, v) in res.headers().iter() {
        builder = builder.header(k, v);
    }
    let body = res.bytes().await?;
    Ok(builder.body(Full::new(body)).unwrap())
}

#[derive(serde::Serialize)]
struct ListedRoom<'a> {
    name: &'a RoomName,
    #[serde(flatten)]
    metadata: &'a RoomMetadata,
}

async fn list_rooms(ctx: Arc<RwLock<BalancerContext>>) -> anyhow::Result<Response<Full<Bytes>>> {
    info!("listing rooms");

    let mut rooms = Vec::new();
    let ctx_read = ctx.read().await;
    for monolith in ctx_read.monoliths.values() {
        let monolith_rooms = monolith.rooms().values();
        for r in monolith_rooms {
            if let Some(meta) = r.metadata() {
                if meta.visibility != Visibility::Public {
                    continue;
                }
                let room = ListedRoom {
                    name: r.name(),
                    metadata: meta,
                };
                rooms.push(room);
            }
        }

        if rooms.len() > 50 {
            break;
        }
    }

    let builder = Response::builder()
        .status(200)
        .header("Content-Type", "application/json");

    let body = serde_json::to_vec(&rooms)?;
    Ok(builder.body(Full::new(body.into())).unwrap())
}
