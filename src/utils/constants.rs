//! DU (SSL) backend endpoints and credentials.
//!
//! Trimmed to what the attendance service actually calls — the course /
//! student-portal constants stayed behind in duerp-api.

/// Exchanges DU credentials for a user object: `POST {SSL_API_ENDPOINT}login`.
pub const LOGIN_ENDPOINT: &str = "login";

// Employee lookup: POST {SSL_API_ENDPOINT}getByEmployeeId with form field
// `employee_id` (exactly 10 digits). 200 = employee, 404 = not an employee.
pub const GET_BY_EMPLOYEE_ID_ENDPOINT: &str = "getByEmployeeId";

// Student lookup: POST {SSL_API_ENDPOINT}get_student_info with form field
// `student_reg_no` (the registration number a card is registered against).
// 200 = student, 404 = no such student, 422 = the field was missing.
//
// POST only — a GET is answered with a 302 to DU's login page, not with JSON.
pub const GET_STUDENT_INFO_ENDPOINT: &str = "get_student_info";

pub const SSL_SECRET_KEY: &str = "4a4cfb4a97000af785115cc9b53c313111e51d9a";