//! A reference a single-record read asks to be expanded (`expand[]=customer`) is the
//! record the session holds under its id, shaped to that record's type, while the record
//! keeps the id; a record the session does not hold is generated once, and stored under
//! the id. A part the contract returns only when expanded (Checkout's `line_items`) is
//! left out of a read that does not ask for it.
#![cfg(feature = "http-rest")]

use serde_json::{Value, json};

use crate::support::{self, HarnessOptions};

/// Shaped as Stripe's contract: expandable references are a required, nullable choice
/// of an id string and the object; a Checkout Session's `line_items` is an optional
/// list its schema names among its `x-expandableFields`, and a customer's
/// `cash_balance` says it is not included by default. An invoice's `customer` is a
/// plain string, a reference by its name alone.
const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Payments, version: "1"}
paths:
  /v1/customers:
    get:
      operationId: GetCustomers
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/customer_list"}}}}}
  /v1/customers/{customer}:
    get:
      operationId: GetCustomersCustomer
      parameters:
        - {name: customer, in: path, required: true, schema: {type: string, maxLength: 5000}}
        - $ref: "#/components/parameters/expand"
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/customer"}}}}}
  /v1/invoices:
    get:
      operationId: GetInvoices
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/invoice_list"}}}}}
  /v1/invoices/{invoice}:
    get:
      operationId: GetInvoicesInvoice
      parameters:
        - {name: invoice, in: path, required: true, schema: {type: string, maxLength: 5000}}
        - $ref: "#/components/parameters/expand"
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/invoice"}}}}}
  /v1/payment_intents:
    get:
      operationId: GetPaymentIntents
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/payment_intent_list"}}}}}
  /v1/payment_intents/{intent}:
    get:
      operationId: GetPaymentIntentsIntent
      parameters:
        - {name: intent, in: path, required: true, schema: {type: string, maxLength: 5000}}
        - $ref: "#/components/parameters/expand"
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/payment_intent"}}}}}
  /v1/checkout/sessions:
    get:
      operationId: GetCheckoutSessions
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/session_list"}}}}}
  /v1/checkout/sessions/{session}:
    get:
      operationId: GetCheckoutSessionsSession
      parameters:
        - {name: session, in: path, required: true, schema: {type: string, maxLength: 66}}
        - $ref: "#/components/parameters/expand"
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/checkout.session"}}}}}
components:
  parameters:
    expand:
      name: expand
      in: query
      style: deepObject
      explode: true
      description: Specifies which fields in the response should be expanded.
      schema: {type: array, items: {type: string, maxLength: 5000}}
  schemas:
    customer:
      type: object
      required: [id, object]
      properties:
        id: {type: string}
        object: {type: string, enum: [customer]}
        email: {type: string, nullable: true}
        cash_balance:
          description: >-
            The cash balance of this customer. This field is not included by default.
            To include it in the response, expand the field.
          nullable: true
          type: object
          properties:
            available: {type: object, additionalProperties: {type: integer}}
    customer_list:
      type: object
      properties:
        object: {type: string, enum: [list]}
        data: {type: array, items: {$ref: "#/components/schemas/customer"}}
        has_more: {type: boolean}
        url: {type: string}
    invoice:
      type: object
      required: [id, object]
      properties:
        id: {type: string}
        object: {type: string, enum: [invoice]}
        amount_due: {type: integer}
        customer: {type: string, nullable: true}
        payment_intent:
          nullable: true
          anyOf:
            - {type: string, maxLength: 5000}
            - $ref: "#/components/schemas/payment_intent"
    invoice_list:
      type: object
      properties:
        object: {type: string, enum: [list]}
        data: {type: array, items: {$ref: "#/components/schemas/invoice"}}
        has_more: {type: boolean}
        url: {type: string}
    payment_intent:
      type: object
      required: [id, object, amount, customer, latest_invoice]
      x-expandableFields: [customer, latest_invoice]
      properties:
        id: {type: string}
        object: {type: string, enum: [payment_intent]}
        amount: {type: integer}
        currency: {type: string}
        customer:
          description: ID of the Customer this PaymentIntent belongs to, if one exists.
          nullable: true
          anyOf:
            - {type: string, maxLength: 5000}
            - $ref: "#/components/schemas/customer"
        latest_invoice:
          nullable: true
          anyOf:
            - {type: string, maxLength: 5000}
            - $ref: "#/components/schemas/invoice"
    payment_intent_list:
      type: object
      properties:
        object: {type: string, enum: [list]}
        data: {type: array, items: {$ref: "#/components/schemas/payment_intent"}}
        has_more: {type: boolean}
        url: {type: string}
    item:
      type: object
      properties:
        id: {type: string}
        object: {type: string, enum: [item]}
        quantity: {type: integer}
    checkout.session:
      type: object
      required: [id, object, mode]
      x-expandableFields: [customer, line_items]
      properties:
        id: {type: string}
        object: {type: string, enum: [checkout.session]}
        mode: {type: string, enum: [payment, setup, subscription]}
        customer:
          nullable: true
          anyOf:
            - {type: string, maxLength: 5000}
            - $ref: "#/components/schemas/customer"
        line_items:
          description: The line items purchased by the customer.
          type: object
          required: [data, has_more, object, url]
          properties:
            data: {type: array, items: {$ref: "#/components/schemas/item"}}
            has_more: {type: boolean}
            object: {type: string, enum: [list]}
            url: {type: string}
    session_list:
      type: object
      properties:
        object: {type: string, enum: [list]}
        data: {type: array, items: {$ref: "#/components/schemas/checkout.session"}}
        has_more: {type: boolean}
        url: {type: string}
"##;

/// The LLM calls made so far.
fn calls(h: &support::DescribedHarness) -> usize {
    h.provider.requests().len()
}

async fn get(h: &support::DescribedHarness, path: &str) -> Value {
    let (status, _, body) = h.send("GET", path, None, &[]).await;
    assert!(status.is_success(), "{path}: {status} {body}");
    body
}

fn customer(id: &str, email: &str) -> Value {
    json!({"id": id, "object": "customer", "email": email})
}

fn intent(id: &str, customer: &str, invoice: Value) -> Value {
    json!({"id": id, "object": "payment_intent", "amount": 1999, "currency": "usd",
           "customer": customer, "latest_invoice": invoice})
}

/// `expand[]=customer` is the customer the session holds; without it, its id. Neither
/// generates, and the intent keeps the id.
#[tokio::test]
async fn an_expanded_reference_is_the_held_record() {
    let h = support::harness(
        CONTRACT,
        vec![
            customer("cus_1", "jenny@example.com"),
            intent("pi_1", "cus_1", Value::Null),
        ],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let held = get(&h, "/v1/customers/cus_1").await;
    assert_eq!(held["email"], "jenny@example.com");
    let plain = get(&h, "/v1/payment_intents/pi_1").await;
    assert_eq!(plain["customer"], "cus_1");
    assert_eq!(calls(&h), 2);

    let expanded = get(&h, "/v1/payment_intents/pi_1?expand[]=customer").await;
    assert_eq!(expanded["customer"], held, "{expanded}");
    assert_eq!(expanded["amount"], 1999);
    // Expansion is a view: the intent still holds the id.
    let again = get(&h, "/v1/payment_intents/pi_1").await;
    assert_eq!(again["customer"], "cus_1", "{again}");
    let repeated = get(&h, "/v1/payment_intents/pi_1?expand[]=customer").await;
    assert_eq!(repeated, expanded);
    assert_eq!(calls(&h), 2);
}

/// `expand[]=latest_invoice.payment_intent` expands each step: the held invoice, and in
/// it the held intent it names, whose own references stay ids.
#[tokio::test]
async fn a_dotted_expansion_expands_each_step() {
    let h = support::harness(
        CONTRACT,
        vec![
            intent("pi_1", "cus_1", Value::Null),
            json!({"id": "in_1", "object": "invoice", "amount_due": 1999,
                   "payment_intent": "pi_1"}),
            intent("pi_2", "cus_1", json!("in_1")),
        ],
        &HarnessOptions::default(),
        |rt| rt,
    );
    get(&h, "/v1/payment_intents/pi_1").await;
    get(&h, "/v1/invoices/in_1").await;
    get(&h, "/v1/payment_intents/pi_2").await;
    assert_eq!(calls(&h), 3);

    let expanded = get(
        &h,
        "/v1/payment_intents/pi_2?expand[]=latest_invoice.payment_intent",
    )
    .await;
    let invoice = &expanded["latest_invoice"];
    assert_eq!(invoice["id"], "in_1", "{expanded}");
    assert_eq!(invoice["amount_due"], 1999);
    assert_eq!(invoice["payment_intent"]["id"], "pi_1", "{expanded}");
    assert_eq!(invoice["payment_intent"]["customer"], "cus_1");
    // Only what was asked: the intent's own customer stays its id.
    assert_eq!(expanded["customer"], "cus_1");

    let plain = get(&h, "/v1/invoices/in_1").await;
    assert_eq!(plain["payment_intent"], "pi_1");
    assert_eq!(calls(&h), 3);
}

/// A reference to a record the session does not hold is generated by the read that
/// expands it, stored under the id the intent names (not the one the model wrote), and
/// every later read answers that record.
#[tokio::test]
async fn an_unheld_reference_is_built_once_and_stored() {
    let h = support::harness(
        CONTRACT,
        vec![
            intent("pi_1", "cus_9", Value::Null),
            json!({"id": "pi_1", "object": "payment_intent", "amount": 5,
                   "latest_invoice": null,
                   "customer": {"id": "cus_model", "object": "customer",
                                "email": "built@example.com"}}),
            json!({"id": "cus_other", "object": "customer", "email": "never@example.com"}),
        ],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let plain = get(&h, "/v1/payment_intents/pi_1").await;
    assert_eq!(plain["customer"], "cus_9");

    let expanded = get(&h, "/v1/payment_intents/pi_1?expand[]=customer").await;
    assert_eq!(calls(&h), 2);
    assert_eq!(
        expanded["customer"],
        customer("cus_9", "built@example.com"),
        "{expanded}"
    );
    // The intent's own values stay as stored.
    assert_eq!(expanded["amount"], 1999);

    let held = get(&h, "/v1/customers/cus_9").await;
    assert_eq!(held, customer("cus_9", "built@example.com"));
    let again = get(&h, "/v1/payment_intents/pi_1?expand[]=customer").await;
    assert_eq!(again, expanded);
    let plain = get(&h, "/v1/payment_intents/pi_1").await;
    assert_eq!(plain["customer"], "cus_9", "{plain}");
    assert_eq!(calls(&h), 2);
}

/// `line_items`, which a Checkout Session answers only when expanded: left out of a
/// read that does not ask for it, the stored one answered to one that does, and one
/// generated once for a session that lacks it.
#[tokio::test]
async fn an_includable_part_is_returned_only_when_expanded() {
    let line_items = json!({"object": "list", "has_more": false,
                            "url": "/v1/checkout/sessions/cs_1/line_items",
                            "data": [{"id": "li_1", "object": "item", "quantity": 2}]});
    let h = support::harness(
        CONTRACT,
        vec![
            json!({"id": "cs_1", "object": "checkout.session", "mode": "payment",
                   "customer": null, "line_items": line_items}),
            json!({"id": "cs_2", "object": "checkout.session", "mode": "payment",
                   "customer": null}),
            json!({"id": "cs_2", "object": "checkout.session", "mode": "payment",
                   "customer": null,
                   "line_items": {"object": "list", "has_more": false,
                                  "url": "/v1/checkout/sessions/cs_2/line_items",
                                  "data": [{"id": "li_9", "object": "item", "quantity": 1}]}}),
        ],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let first = get(&h, "/v1/checkout/sessions/cs_1").await;
    assert!(first.get("line_items").is_none(), "{first}");
    let expanded = get(&h, "/v1/checkout/sessions/cs_1?expand[]=line_items").await;
    assert_eq!(expanded["line_items"], line_items, "{expanded}");
    assert_eq!(calls(&h), 1);

    let bare = get(&h, "/v1/checkout/sessions/cs_2").await;
    assert!(bare.get("line_items").is_none(), "{bare}");
    let built = get(&h, "/v1/checkout/sessions/cs_2?expand[]=line_items").await;
    assert_eq!(built["line_items"]["data"][0]["id"], "li_9", "{built}");
    assert_eq!(calls(&h), 3);
    let again = get(&h, "/v1/checkout/sessions/cs_2?expand[]=line_items").await;
    assert_eq!(again["line_items"], built["line_items"]);
    let bare = get(&h, "/v1/checkout/sessions/cs_2").await;
    assert!(bare.get("line_items").is_none(), "{bare}");
    assert_eq!(calls(&h), 3);
}

/// A member whose description says it is not included by default is left out of a read
/// that does not expand it, and of the record when another read expands it.
#[tokio::test]
async fn a_part_not_included_by_default_is_left_out_unless_expanded() {
    let h = support::harness(
        CONTRACT,
        vec![
            json!({"id": "cus_1", "object": "customer", "email": "jenny@example.com",
                   "cash_balance": {"available": {"usd": 500}}}),
            intent("pi_1", "cus_1", Value::Null),
        ],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let plain = get(&h, "/v1/customers/cus_1").await;
    assert!(plain.get("cash_balance").is_none(), "{plain}");
    let expanded = get(&h, "/v1/customers/cus_1?expand[]=cash_balance").await;
    assert_eq!(
        expanded["cash_balance"]["available"]["usd"], 500,
        "{expanded}"
    );
    get(&h, "/v1/payment_intents/pi_1").await;
    let intent = get(&h, "/v1/payment_intents/pi_1?expand[]=customer").await;
    assert_eq!(intent["customer"], plain, "{intent}");
    assert_eq!(calls(&h), 2);
}

/// A string member named after a type (an invoice's `customer`) expands into the record
/// the session holds under its value; one naming no held record stays the value, and
/// nothing is generated for it.
#[tokio::test]
async fn a_reference_by_name_expands_a_held_record() {
    let h = support::harness(
        CONTRACT,
        vec![
            customer("cus_1", "jenny@example.com"),
            json!({"id": "in_1", "object": "invoice", "amount_due": 10,
                   "customer": "cus_1", "payment_intent": null}),
            json!({"id": "in_2", "object": "invoice", "amount_due": 20,
                   "customer": "cus_404", "payment_intent": null}),
        ],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let held = get(&h, "/v1/customers/cus_1").await;
    get(&h, "/v1/invoices/in_1").await;
    get(&h, "/v1/invoices/in_2").await;
    let expanded = get(&h, "/v1/invoices/in_1?expand[]=customer").await;
    assert_eq!(expanded["customer"], held, "{expanded}");
    let plain = get(&h, "/v1/invoices/in_1").await;
    assert_eq!(plain["customer"], "cus_1");
    let unheld = get(&h, "/v1/invoices/in_2?expand[]=customer").await;
    assert_eq!(unheld["customer"], "cus_404", "{unheld}");
    assert_eq!(calls(&h), 3);
}

/// A generated read whose answer holds a referenced record in place of its id stores
/// that record under its id, and the read and its record hold the id.
#[tokio::test]
async fn a_generated_reference_object_is_stored_and_answered_as_its_id() {
    let h = support::harness(
        CONTRACT,
        vec![
            json!({"id": "pi_5", "object": "payment_intent", "amount": 700,
                    "customer": customer("cus_5", "five@example.com"),
                    "latest_invoice": null}),
        ],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let plain = get(&h, "/v1/payment_intents/pi_5").await;
    assert_eq!(plain["customer"], "cus_5", "{plain}");
    let held = get(&h, "/v1/customers/cus_5").await;
    assert_eq!(held, customer("cus_5", "five@example.com"));
    let expanded = get(&h, "/v1/payment_intents/pi_5?expand[]=customer").await;
    assert_eq!(expanded["customer"], held);
    assert_eq!(get(&h, "/v1/payment_intents/pi_5").await, plain);
    assert_eq!(calls(&h), 1);
}

/// A read expanding a reference to a record the session does not hold, whose answer
/// generates no record there, has one drawn under the id: the next read agrees, and
/// generates nothing.
#[tokio::test]
async fn an_unheld_reference_left_ungenerated_is_drawn_once() {
    let h = support::harness(
        CONTRACT,
        vec![
            intent("pi_7", "cus_7", Value::Null),
            intent("pi_7", "cus_7", Value::Null),
        ],
        &HarnessOptions::default(),
        |rt| rt,
    );
    get(&h, "/v1/payment_intents/pi_7").await;
    let expanded = get(&h, "/v1/payment_intents/pi_7?expand[]=customer").await;
    assert_eq!(expanded["customer"]["id"], "cus_7", "{expanded}");
    assert_eq!(calls(&h), 2);
    let again = get(&h, "/v1/payment_intents/pi_7?expand[]=customer").await;
    assert_eq!(again, expanded);
    let held = get(&h, "/v1/customers/cus_7").await;
    assert_eq!(held["id"], "cus_7");
    assert_eq!(
        get(&h, "/v1/payment_intents/pi_7").await["customer"],
        "cus_7"
    );
    assert_eq!(calls(&h), 2);
}
