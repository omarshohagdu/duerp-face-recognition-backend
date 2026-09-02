use std::env;
use std::future::{ready, Ready};
use std::rc::Rc;
use std::task::{Context, Poll};

use actix_service::{Service, Transform};
use actix_web::body::{EitherBody, MessageBody};
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::{web, Error, HttpResponse};
use futures_util::future::LocalBoxFuture;
use sqlx::PgPool;

pub struct ExtAuthMiddleware;

impl<S, B> Transform<S, ServiceRequest> for ExtAuthMiddleware
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Transform = ExtAuthMiddlewareMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        let app_id = env::var("EXT_APP_ID").expect("EXT_APP_ID must be set in .env");
        let app_password = env::var("EXT_APP_PASSWORD").expect("EXT_APP_PASSWORD must be set in .env");
        ready(Ok(ExtAuthMiddlewareMiddleware {
            service: Rc::new(service),
            app_id,
            app_password,
        }))
    }
}

pub struct ExtAuthMiddlewareMiddleware<S> {
    service: Rc<S>,
    app_id: String,
    app_password: String,
}

impl<S, B> Service<ServiceRequest> for ExtAuthMiddlewareMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(ctx)
    }

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let svc = Rc::clone(&self.service);
        let expected_id = self.app_id.clone();
        let expected_password = self.app_password.clone();

        // Get DB pool from app_data
        let db = req.app_data::<web::Data<PgPool>>().cloned();

        // Capture endpoint path and client IP before moving req
        let endpoint = req.path().to_string();
        let client_ip = req
            .connection_info()
            .realip_remote_addr()
            .unwrap_or("")
            .split(':')
            .next()
            .unwrap_or("")
            .to_string();
        Box::pin(async move {
            // Credential check
            let app_id = req
                .headers()
                .get("X-App-Id")
                .and_then(|h| h.to_str().ok());

            let app_password = req
                .headers()
                .get("X-App-Password")
                .and_then(|h| h.to_str().ok());

            if app_id != Some(&expected_id) || app_password != Some(&expected_password) {
                let res = HttpResponse::Unauthorized()
                    .json(serde_json::json!({ "error": "Invalid App ID or Password" }))
                    .map_into_right_body();
                return Ok(req.into_response(res));
            }

            // IP check against database
            if let Some(pool) = db {
                let allowed: bool = sqlx::query_scalar(
                    r#"
                    SELECT EXISTS (
                        SELECT 1 FROM ictcell.ext_api_allowed_ips
                        WHERE endpoint = $1
                          AND $2 = ANY(ip_address)
                          AND is_active = true
                    )
                    "#,
                )
                .bind(&endpoint)
                .bind(&client_ip)
                .fetch_one(pool.get_ref())
                .await
                .unwrap_or(false);

                if !allowed {
                    let res = HttpResponse::Forbidden()
                        .json(serde_json::json!({
                            "error": format!("IP address not allowed for this endpoint. Add your requested IP address: '{}'", client_ip)
                        }))
                        .map_into_right_body();
                    return Ok(req.into_response(res));
                }
            }

            let res = svc.call(req).await?;
            Ok(res.map_into_left_body())
        })
    }
}
