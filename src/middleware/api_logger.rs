use std::future::{ready, Ready};
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::Instant;

use actix_web::body::{BoxBody, MessageBody};
use actix_web::dev::{Service, ServiceRequest, ServiceResponse, Transform};
use actix_web::{web, Error, HttpResponse};
use futures_util::future::LocalBoxFuture;
use futures_util::TryStreamExt;
use serde_json::Value;
use sqlx::PgPool;

pub struct ApiLogger;

impl<S, B> Transform<S, ServiceRequest> for ApiLogger
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: MessageBody + 'static,
    B::Error: Into<Box<dyn std::error::Error>>,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = Error;
    type Transform = ApiLoggerMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(ApiLoggerMiddleware {
            service: Rc::new(service),
        }))
    }
}

pub struct ApiLoggerMiddleware<S> {
    service: Rc<S>,
}

impl<S, B> Service<ServiceRequest> for ApiLoggerMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: MessageBody + 'static,
    B::Error: Into<Box<dyn std::error::Error>>,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(ctx)
    }

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let svc = Rc::clone(&self.service);

        Box::pin(async move {
            let start = Instant::now();

            let method = req.method().to_string();
            let endpoint = req.path().to_string();
            let client_ip = req
                .connection_info()
                .realip_remote_addr()
                .unwrap_or("")
                .split(':')
                .next()
                .unwrap_or("")
                .to_string();
            let user_agent = req
                .headers()
                .get("User-Agent")
                .and_then(|h| h.to_str().ok())
                .unwrap_or("")
                .to_string();
            let db = req.app_data::<web::Data<PgPool>>().cloned();

            // Buffer request body then reconstruct payload so handler can still read it
            let (http_req, payload) = req.into_parts();
            let body_bytes = payload
                .try_fold(web::BytesMut::new(), |mut acc, chunk| async move {
                    acc.extend_from_slice(&chunk);
                    Ok(acc)
                })
                .await
                .map(|b| b.freeze())
                .unwrap_or_default();

            let request_body: Value =
                serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);

            let req = ServiceRequest::from_parts(
                http_req,
                actix_web::dev::Payload::from(body_bytes),
            );

            // Call the handler
            let res = svc.call(req).await?;
            let status_code = res.status().as_u16();
            let duration_ms = start.elapsed().as_millis() as i32;

            // Buffer response body then rebuild response with the same bytes
            let (req_parts, http_response) = res.into_parts();
            let resp_bytes = actix_web::body::to_bytes(http_response.into_body())
                .await
                .unwrap_or_default();

            let response_body: Value = serde_json::from_slice(&resp_bytes)
                .unwrap_or_else(|_| {
                    Value::String(String::from_utf8_lossy(&resp_bytes).into_owned())
                });

            let error_message: Option<String> = if status_code >= 400 {
                response_body
                    .get("error")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .or_else(|| response_body.as_str().map(String::from))
            } else {
                None
            };

            // Fire-and-forget insert — never blocks the response
            if let Some(pool) = db {
                tokio::spawn(async move {
                    let result = sqlx::query(
                        r#"
                        INSERT INTO ictcell.ext_api_call_logs
                            (endpoint, method, request_body, response_body,
                             status_code, duration_ms, client_ip, user_agent, error_message)
                        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                        "#,
                    )
                    .bind(endpoint)
                    .bind(method)
                    .bind(request_body)
                    .bind(response_body)
                    .bind(status_code as i16)
                    .bind(duration_ms)
                    .bind(client_ip)
                    .bind(user_agent)
                    .bind(error_message)
                    .execute(pool.get_ref())
                    .await;

                    if let Err(e) = result {
                        eprintln!("API logger DB insert error: {e}");
                    }
                });
            }

            let new_status = actix_web::http::StatusCode::from_u16(status_code)
                .unwrap_or(actix_web::http::StatusCode::INTERNAL_SERVER_ERROR);

            Ok(ServiceResponse::new(
                req_parts,
                HttpResponse::build(new_status).body(resp_bytes),
            ))
        })
    }
}
