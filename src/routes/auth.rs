use std::env;
use crate::models::auth::{LoginRequest, LoginResponse};
use crate::utils::jwt::create_jwt;
use actix_web::{post, web, HttpResponse, Responder};
use reqwest::Client;
use serde_json::Value;
use sqlx::PgPool;
use crate::utils::constants::{LOGIN_ENDPOINT, SSL_SECRET_KEY};

#[post("/login")]
pub async fn login(req: web::Json<LoginRequest>, db: web::Data<PgPool>) -> impl Responder {
    let client = Client::new();

    // Prepare form data for external API
    let form_data = [
        ("email", req.username.clone()),
        ("password", req.password.clone()),
    ];

    // Call external login API
    let response = client
        .post(env::var("SSL_API_ENDPOINT").unwrap() + LOGIN_ENDPOINT)
        .header("secret-key", SSL_SECRET_KEY)
        .form(&form_data)
        .send()
        .await;

    match response {
        Ok(resp) => {
            if resp.status().is_success() {
                // Deserialize DU API response
                let api_json: Value = match resp.json().await {
                    Ok(json) => json,
                    Err(_) => {
                        return HttpResponse::InternalServerError().json("Invalid API response");
                    }
                };

                //println!("DU API response: {}", api_json);
                if let Some(user_obj) = api_json.get("user") {
                    let username = user_obj["username"].as_str().unwrap_or("unknown").to_string();
                    let user_id = user_obj["user_id"].as_u64().unwrap_or(0);
                    let user_data = user_obj.clone();

                    // Choose the person_id that goes into the token's `sub`.
                    // For non-students the person_id is the employee id
                    // (`user.emp_id`, e.g. "2020111007"); students keep `user_id`.
                    // wow-attendance enroll/verify match this `sub` against the
                    // supplied person_id.
                    let user_role = user_obj["user_role"].as_str().unwrap_or("");
                    let person_id: u64 = if user_role.eq_ignore_ascii_case("student") {
                        user_id
                    } else {
                        user_obj["emp_id"]
                            .as_str()
                            .and_then(|s| s.trim().parse::<u64>().ok())
                            .unwrap_or(user_id)
                    };

                    // create your own internal JWT (local token)
                    let token = create_jwt(person_id);

                    return HttpResponse::Ok().json(LoginResponse {
                        token,
                        username,
                        user_data,
                    });
                }

                HttpResponse::Unauthorized().json("Invalid user data from external API")
            } else {
                HttpResponse::Unauthorized().json("Login failed at external API")
            }
        }
        Err(err) => {
            eprintln!("External login error: {:?}", err);
            HttpResponse::InternalServerError().json("Login service unavailable")
        }
    }
}
