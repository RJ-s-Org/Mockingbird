# Harbor Stays API

Harbor Stays lets partner sites search for hotel rooms, read property details, and
create, look up and cancel reservations. Every request and response is JSON over HTTPS.
The base URL is `https://api.harborstays.example`. Authenticate by sending your secret
key in the `Authorization` header as `Bearer <key>`. Keys that start with `hs_test_`
reach the sandbox, where no real card is charged.

Dates are ISO 8601 calendar dates (`2026-12-18`). Timestamps are RFC 3339 in UTC
(`2026-12-01T09:30:00Z`). Money is an object with an `amount` string such as `"189.00"`
and an ISO 4217 `currency` code such as `EUR`. Amounts are never sent as numbers.

## Searching for properties

Send a GET request to `/v1/properties` to list the hotels in a city. The `city` query
parameter is required and takes a three-letter IATA city code such as `LIS`. Pass
`check_in` and `check_out` as dates; `check_out` must be later than `check_in`, and the
stay may not exceed 28 nights. `guests` is optional, defaults to 2, and accepts 1 to 9.
Use `page` (starting at 1) and `page_size` (1 to 100, default 20) to page through
results.

The response is a page object with `data`, an array of properties, plus `page`,
`page_size` and `total`. Each property has an `id` (a UUID), a `name`, a `star_rating`
from 1 to 5, a guest `rating` from 0 to 10, an `address` with `line1`, `city`,
`postal_code`, `country_code` (ISO 3166-1 alpha-2) and `geo` coordinates, a list of
`amenities` (`wifi`, `pool`, `spa`, `gym`, `parking`, `pet_friendly`, `restaurant`,
`airport_shuttle`), a `website` URL, a local `check_in_time` in 24-hour HH:MM form, and
the `cheapest_rate` for the requested stay with `per_night`, `currency` and
`breakfast_included`.

## Reading one property

Send a GET request to `/v1/properties/{property_id}` with the property's UUID. The
response has the same fields as a search result, plus `room_types`, an array in which
each room type has its own `id`, `name`, `max_guests`, `bed_type` (`single`, `double`,
`queen`, `king`, `twin`) and a `rates` array. Each rate lists `rate_plan` (`flexible` or
`non_refundable`), `total` as a money object, and `free_cancellation_until`, a timestamp
that is null for non-refundable rates. If the property ID is unknown the API returns
`404` with a problem document.

## Creating a reservation

Send a POST request to `/v1/reservations` with a JSON body. It needs `room_type_id`,
`rate_plan`, `check_in`, `check_out`, a `guests` array where each guest has `given_name`,
`family_name` and `email`, and a `payment` object. The payment object is either a card
(`type` set to `card`, with `token` from our card form) or a bank transfer (`type` set
to `bank_transfer`, with an `iban` in the EU format). Send an `Idempotency-Key` header
with each create request; repeating a key returns the first reservation instead of
making a second one.

A successful create returns `201` with the new reservation: its `id`, a six-character
`confirmation_code` made of capital letters and digits, `status` (one of `pending`,
`confirmed`, `cancelled` or `failed`), `created_at` and `updated_at` timestamps, the
property and room type, the guests, the `total` money object, and the `payment` method
used, echoed back without the card token. Bank transfers start as `pending` until funds
arrive.

## Retrieving a reservation

Send a GET request to `/v1/reservations/{reservation_id}`. The response is the
reservation object described above. Reservations are kept for seven years, so older
records can still be read.

## Cancelling a reservation

Send a POST request to `/v1/reservations/{reservation_id}/cancellations`. The body is
optional. Pass `reason` as one of `guest_request`, `schedule_change` or `payment_failed`.
The API cancels the reservation and answers `200` with a cancellation object containing
`reservation_id`, `status` (`refunded` once the money is back, or `refund_pending` while
the bank processes it), `cancelled_at`, and a `refund` object with the `amount` returned,
any cancellation `fee`, and an `expected_by` date. Cancelling a reservation that is
already cancelled returns `409`.

## Errors

Errors use `application/problem+json`. Each problem has a `type` URI, a short `title`,
the HTTP `status`, and a `detail` sentence that is safe to show to a guest. Validation
failures return `422`. Rate limits return `429` with a `Retry-After` header in seconds.
