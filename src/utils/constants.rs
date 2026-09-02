//! DU (SSL) backend endpoints and credentials.
//!
//! Trimmed to what the attendance service actually calls — the course /
//! student-portal constants stayed behind in duerp-api.

/// Exchanges DU credentials for a user object: `POST {SSL_API_ENDPOINT}login`.
pub const LOGIN_ENDPOINT: &str = "login";

// Employee lookup: POST {SSL_API_ENDPOINT}getByEmployeeId with form field
// `employee_id` (exactly 10 digits). 200 = employee, 404 = not an employee.
pub const GET_BY_EMPLOYEE_ID_ENDPOINT: &str = "getByEmployeeId";

pub const SSL_SECRET_KEY: &str = "4a4cfb4a97000af785115cc9b53c313111e51d9a";