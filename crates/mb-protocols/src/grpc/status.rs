//! HTTP status ↔ gRPC status code mapping.

use tonic::{Code, Status};

use crate::call::PlanError;

/// The gRPC code standing for HTTP `status`.
pub fn grpc_code_for_status(status: u16) -> Code {
    match status {
        401 => Code::Unauthenticated,
        403 => Code::PermissionDenied,
        404 => Code::NotFound,
        409 => Code::AlreadyExists,
        412 => Code::FailedPrecondition,
        416 => Code::OutOfRange,
        429 => Code::ResourceExhausted,
        499 => Code::Cancelled,
        501 => Code::Unimplemented,
        503 => Code::Unavailable,
        504 => Code::DeadlineExceeded,
        // 400 and any other 4xx.
        400..=499 => Code::InvalidArgument,
        _ => Code::Internal,
    }
}

/// A call's failure: the HTTP status it is reported with and the gRPC
/// status the client receives.
pub(crate) struct Failure {
    pub(crate) http: u16,
    pub(crate) status: Status,
}

impl Failure {
    pub(crate) fn new(http: u16, message: impl Into<String>) -> Self {
        Self {
            http,
            status: Status::new(grpc_code_for_status(http), message),
        }
    }

    /// A non-2xx status forced by a header or scenario.
    pub(crate) fn forced(http: u16) -> Self {
        Self::new(http, format!("forced by x-mockingbird-status: {http}"))
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(500, message)
    }

    /// A failed [`crate::call::plan`] of mock `mock`.
    pub(crate) fn plan(error: PlanError, mock: &str) -> Self {
        match error {
            PlanError::Rejected(r) | PlanError::RateLimited(r) => Self::new(r.status, r.detail),
            PlanError::Unauthorized(u) => Self::new(401, u.detail),
            PlanError::BadOverride(detail) => Self::new(400, detail),
            PlanError::UnknownScenario(name) => {
                Self::new(400, format!("mock {mock} has no scenario {name:?}"))
            }
        }
    }

    /// A status tonic produced while reading the request (an undecodable
    /// or oversized message, a broken stream), reported as a 400; an
    /// oversized message is `RESOURCE_EXHAUSTED`, reported as a 413.
    pub(crate) fn request(status: Status) -> Self {
        match oversized(&status) {
            Some(status) => Self { http: 413, status },
            None => Self { http: 400, status },
        }
    }
}

/// tonic answers a message over the decoding limit with `OUT_OF_RANGE`;
/// gRPC's own servers (and Mockingbird) use `RESOURCE_EXHAUSTED`. The
/// replacement status, when `status` is that refusal.
pub(crate) fn oversized(status: &Status) -> Option<Status> {
    (status.code() == Code::OutOfRange
        && status
            .message()
            .contains("decoded message length too large"))
    .then(|| Status::resource_exhausted(status.message()))
}
