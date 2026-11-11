//! Operations that are not REST routes get their kind, type and target from their
//! structure: the verb an RPC route or operation name ends or starts with, the request
//! member that names the record, and the member of the answer that holds it. What the
//! structure leaves ambiguous stays unclassified.
#![cfg(test)]

use mb_import::compile;
use mb_import::world::tier0;
use mb_mcm::world::{OpKind, Pointer};
use mb_mcm::{Bundle, Effect, Identity, OperationKind, RecordAt};

fn op<'a>(bundle: &'a Bundle, id: &str) -> &'a mb_mcm::Operation {
    bundle
        .operation(id)
        .unwrap_or_else(|| panic!("no operation {id}"))
}

fn identity(field: &str, input_field: Option<&str>, record: Option<RecordAt>) -> Option<Identity> {
    Some(Identity {
        path_param: None,
        field: field.into(),
        input_field: input_field.map(str::to_owned),
        path_field: None,
        record,
    })
}

/// A Slack-style API: every call a `POST` to `<type>.<verb>`, the record named by a
/// body member, the answer an `{ok, <record>}` envelope.
const DOTTED: &str = r##"
openapi: 3.0.0
info: {title: Rpc, version: "1"}
paths:
  /api/widgets.create:
    post:
      operationId: widgetsCreate
      requestBody: {content: {application/json: {schema: {type: object, required: [name], properties: {name: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/WidgetAnswer"}}}}}
  /api/widgets.info:
    post:
      operationId: widgetsInfo
      requestBody: {content: {application/json: {schema: {type: object, required: [widget], properties: {widget: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/WidgetAnswer"}}}}}
  /api/widgets.update:
    post:
      operationId: widgetsUpdate
      requestBody: {content: {application/json: {schema: {type: object, required: [widget_id], properties: {widget_id: {type: string}, name: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/WidgetAnswer"}}}}}
  /api/widgets.delete:
    post:
      operationId: widgetsDelete
      requestBody: {content: {application/json: {schema: {type: object, required: [id], properties: {id: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {ok: {type: boolean}}}}}}}
  /api/widgets.list:
    post:
      operationId: widgetsList
      requestBody: {content: {application/json: {schema: {type: object, properties: {cursor: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {ok: {type: boolean}, widgets: {type: array, items: {$ref: "#/components/schemas/Widget"}}}}}}}}
  /api/widgets.search:
    post:
      operationId: widgetsSearch
      requestBody: {content: {application/json: {schema: {type: object, properties: {id: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/WidgetAnswer"}}}}}
components:
  schemas:
    Widget:
      type: object
      required: [id, name]
      properties: {id: {type: string}, name: {type: string}}
    WidgetAnswer:
      type: object
      required: [ok, widget]
      properties: {ok: {type: boolean}, widget: {$ref: "#/components/schemas/Widget"}}
"##;

#[test]
fn a_dotted_rpc_route_is_classified_by_its_verb_and_its_body_member() {
    let b = compile(DOTTED.as_bytes()).unwrap();
    let envelope = Some(RecordAt::Member("widget".into()));
    let create = op(&b, "widgetsCreate");
    assert_eq!(create.kind, OperationKind::Command(Effect::Create));
    assert_eq!(create.resource_type.as_deref(), Some("widgets"));
    assert_eq!(create.identity, identity("id", None, envelope.clone()));

    // A singular read whose body member is named for the type reads one record.
    let info = op(&b, "widgetsInfo");
    assert_eq!(info.kind, OperationKind::Query);
    assert_eq!(info.resource_type.as_deref(), Some("widgets"));
    assert_eq!(
        info.identity,
        identity("id", Some("widget"), envelope.clone())
    );

    let update = op(&b, "widgetsUpdate");
    assert_eq!(update.kind, OperationKind::Command(Effect::Update));
    assert_eq!(update.resource_type.as_deref(), Some("widgets"));
    assert_eq!(update.identity, identity("id", Some("widget_id"), envelope));

    // A delete answering an acknowledgement: the record is not in its answer.
    let delete = op(&b, "widgetsDelete");
    assert_eq!(delete.kind, OperationKind::Command(Effect::Delete));
    assert_eq!(delete.resource_type.as_deref(), Some("widgets"));
    assert_eq!(
        delete
            .identity
            .as_ref()
            .and_then(|i| i.input_field.as_deref()),
        Some("id")
    );

    // Lists and searches take no target, whatever their body holds.
    for id in ["widgetsList", "widgetsSearch"] {
        let o = op(&b, id);
        assert_eq!(o.kind, OperationKind::Query, "{id}");
        assert_eq!(o.resource_type.as_deref(), Some("widgets"), "{id}");
        assert_eq!(
            o.identity.as_ref().and_then(|i| i.input_field.as_deref()),
            None,
            "{id}"
        );
    }
    assert_eq!(op(&b, "widgetsList").identity, None);

    // Tier 0 restates it: the kinds, one type, and the body member as the target.
    let world = tier0(&b);
    let desc = |id: &str| &world.operations[id];
    assert_eq!(desc("widgetsInfo").kind, Some(OpKind::Read));
    assert_eq!(desc("widgetsList").kind, Some(OpKind::List));
    assert_eq!(desc("widgetsDelete").kind, Some(OpKind::Delete));
    assert_eq!(
        desc("widgetsUpdate")
            .target
            .as_ref()
            .and_then(|t| t.member.as_deref()),
        Some("widget_id")
    );
    assert_eq!(
        desc("widgetsInfo")
            .answer
            .as_ref()
            .and_then(|a| a.record.clone()),
        Some(Pointer::parse("/widget").unwrap())
    );
    assert!(
        world
            .operations
            .values()
            .all(|d| d.r#type.as_deref() == Some("widgets")),
        "{world:?}"
    );
}

/// A Dropbox- or Plaid-style API: the verb is a path segment of its own after the
/// type, possibly with a suffix (`delete_v2`); records are named by what no rule can
/// tell is their id (`path`).
const SEGMENTED: &str = r##"
openapi: 3.0.0
info: {title: Rpc, version: "1"}
paths:
  /2/files/create_folder_v2:
    post:
      operationId: createFolder
      requestBody: {content: {application/json: {schema: {type: object, required: [path], properties: {path: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {metadata: {$ref: "#/components/schemas/Entry"}}}}}}}
  /2/files/get_metadata:
    post:
      operationId: getMetadata
      requestBody: {content: {application/json: {schema: {type: object, required: [path], properties: {path: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Entry"}}}}}
  /2/files/delete_v2:
    post:
      operationId: delete
      requestBody: {content: {application/json: {schema: {type: object, required: [path], properties: {path: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {metadata: {$ref: "#/components/schemas/Entry"}}}}}}}
  /link/token/create:
    post:
      operationId: createLinkToken
      requestBody: {content: {application/json: {schema: {type: object, properties: {user: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {link_token: {type: string}}}}}}}
components:
  schemas:
    Entry:
      type: object
      required: [id, path_lower]
      properties: {id: {type: string}, path_lower: {type: string}}
"##;

#[test]
fn a_verb_segment_names_the_write_and_the_segment_before_it_the_type() {
    let b = compile(SEGMENTED.as_bytes()).unwrap();
    let folder = op(&b, "createFolder");
    assert_eq!(folder.kind, OperationKind::Command(Effect::Create));
    assert_eq!(folder.resource_type.as_deref(), Some("files"));
    let delete = op(&b, "delete");
    assert_eq!(delete.kind, OperationKind::Command(Effect::Delete));
    assert_eq!(delete.resource_type.as_deref(), Some("files"));
    // `path` is not named for the record's key: no target.
    assert_eq!(delete.identity, None);
    let read = op(&b, "getMetadata");
    assert_eq!(read.kind, OperationKind::Query);
    assert!(
        read.identity
            .as_ref()
            .is_none_or(|i| i.input_field.is_none()),
        "{:?}",
        read.identity
    );
    let token = op(&b, "createLinkToken");
    assert_eq!(token.kind, OperationKind::Command(Effect::Create));
    assert_eq!(token.resource_type.as_deref(), Some("token"));
}

/// A route that acts on a record by its path parameter with a write verb for its last
/// segment is that write: Chargebee's `POST /customers/{id}/delete`.
#[test]
fn an_action_route_named_for_a_write_is_that_write() {
    let contract = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /comments/{comment_id}:
    parameters: [{name: comment_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getComment
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/CommentAnswer"}}}}}
  /comments/{comment_id}/delete:
    parameters: [{name: comment_id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: deleteComment
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/CommentAnswer"}}}}}
  /comments/{comment_id}/close:
    parameters: [{name: comment_id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: closeComment
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/CommentAnswer"}}}}}
components:
  schemas:
    Comment: {type: object, required: [id], properties: {id: {type: string}, body: {type: string}}}
    CommentAnswer: {type: object, required: [comment], properties: {comment: {$ref: "#/components/schemas/Comment"}}}
"##;
    let b = compile(contract.as_bytes()).unwrap();
    let delete = op(&b, "deleteComment");
    assert_eq!(delete.kind, OperationKind::Command(Effect::Delete));
    assert_eq!(delete.resource_type.as_deref(), Some("comments"));
    let target = delete.identity.as_ref().unwrap();
    assert_eq!(target.path_param.as_deref(), Some("comment_id"));
    // Any other verb is what it was.
    assert_eq!(
        op(&b, "closeComment").kind,
        OperationKind::Command(Effect::Update)
    );
}

const LINEAR: &str = r#"
type Issue { id: ID! title: String! }
type Comment { id: ID! body: String! }
type IssuePayload { success: Boolean! issue: Issue }
type IssueArchivePayload { success: Boolean! entity: Issue }
type CommentPayload { success: Boolean! comment: Comment! }
type DeletePayload { success: Boolean! entityId: String! }
input IssueCreateInput { title: String! }
input IssueUpdateInput { title: String }
input CommentCreateInput { body: String! issueId: String! }
type Query { issue(id: String!): Issue! }
type Mutation {
  issueCreate(input: IssueCreateInput!): IssuePayload!
  issueUpdate(id: String!, input: IssueUpdateInput!): IssuePayload!
  issueArchive(id: String!): IssueArchivePayload!
  commentCreate(input: CommentCreateInput!): CommentPayload!
  commentDelete(id: String!): DeletePayload!
  deleteIssues(ids: [String!]!): DeletePayload!
}
"#;

#[test]
fn a_graphql_create_or_delete_of_a_record_is_identified_by_its_payload_and_id_argument() {
    let b = compile(LINEAR.as_bytes()).unwrap();
    let issue = Some(RecordAt::Member("issue".into()));
    let create = op(&b, "Mutation.issueCreate");
    assert_eq!(create.kind, OperationKind::Command(Effect::Create));
    assert_eq!(create.resource_type.as_deref(), Some("issue"));
    assert_eq!(create.identity, identity("id", None, issue.clone()));
    // An update's arguments are not its record: the GraphQL runtime applies its
    // `input` by name, so it keeps no identity.
    let update = op(&b, "Mutation.issueUpdate");
    assert_eq!(update.kind, OperationKind::Command(Effect::Update));
    assert_eq!(update.identity, None);
    let comment = op(&b, "Mutation.commentCreate");
    assert_eq!(
        comment.identity,
        identity("id", None, Some(RecordAt::Member("comment".into())))
    );
    // A delete's payload need not hold the record: its id argument names it.
    let delete = op(&b, "Mutation.commentDelete");
    assert_eq!(delete.kind, OperationKind::Command(Effect::Delete));
    assert_eq!(delete.resource_type.as_deref(), Some("comment"));
    assert_eq!(
        delete
            .identity
            .as_ref()
            .and_then(|i| i.input_field.as_deref()),
        Some("id")
    );
    // Not a write verb, or no one record named: unclassified as before.
    assert_eq!(op(&b, "Mutation.issueArchive").identity, None);
    assert_eq!(op(&b, "Mutation.deleteIssues").identity, None);

    let world = tier0(&b);
    let create = &world.operations["Mutation.issueCreate"];
    assert_eq!(create.kind, Some(OpKind::Create));
    assert_eq!(create.r#type.as_deref(), Some("issue"));
    assert_eq!(
        create.answer.as_ref().and_then(|a| a.record.clone()),
        Some(Pointer::parse("/issue").unwrap())
    );
    let delete = &world.operations["Mutation.commentDelete"];
    assert_eq!(delete.kind, Some(OpKind::Delete));
    assert_eq!(
        delete.target.as_ref().and_then(|t| t.member.as_deref()),
        Some("id")
    );
}

const STOREFRONT: &str = r#"
type ProductVariant { id: ID! }
type CartLine { id: ID! quantity: Int! merchandise: ProductVariant! }
type Cart { id: ID! lines: [CartLine!]! }
type CartUserError { message: String! }
type CartCreatePayload { cart: Cart userErrors: [CartUserError!]! }
type CartLinesAddPayload { cart: Cart userErrors: [CartUserError!]! }
type CartLinesUpdatePayload { cart: Cart userErrors: [CartUserError!]! }
input CartLineInput { merchandiseId: ID! quantity: Int }
input CartLineUpdateInput { id: ID! quantity: Int }
type Query { cart(id: ID!): Cart }
type Mutation {
  cartCreate: CartCreatePayload
  cartLinesAdd(cartId: ID!, lines: [CartLineInput!]!): CartLinesAddPayload
  cartLinesUpdate(cartId: ID!, lines: [CartLineUpdateInput!]!): CartLinesUpdatePayload
}
"#;

/// A mutation on part of the record its payload holds (Shopify's cart lines) acts on
/// that record, named by its `<type>Id` argument, not on its payload type or on the
/// part its name names.
#[test]
fn a_graphql_mutation_acts_on_the_record_its_payload_holds() {
    let b = compile(STOREFRONT.as_bytes()).unwrap();
    for name in ["Mutation.cartLinesAdd", "Mutation.cartLinesUpdate"] {
        let o = op(&b, name);
        assert_eq!(o.kind, OperationKind::Command(Effect::Other), "{name}");
        assert_eq!(o.resource_type.as_deref(), Some("cart"), "{name}");
        let identity = o.identity.as_ref().unwrap();
        assert_eq!(identity.input_field.as_deref(), Some("cartId"), "{name}");
        assert_eq!(identity.record, Some(RecordAt::Member("cart".into())));
    }
    // A create of the record is what it was.
    assert_eq!(
        op(&b, "Mutation.cartCreate").kind,
        OperationKind::Command(Effect::Create)
    );
    let world = tier0(&b);
    let add = &world.operations["Mutation.cartLinesAdd"];
    assert_eq!(add.kind, Some(OpKind::Action));
    assert_eq!(add.r#type.as_deref(), Some("cart"));
    assert_eq!(
        add.target.as_ref().and_then(|t| t.member.as_deref()),
        Some("cartId")
    );
}

/// An eBay Trading-style WSDL: the record is a member named for the type, keyed by
/// `<Type>ID`; the create sends the record whole and answers its id.
const TRADING: &str = r#"<?xml version="1.0"?>
<wsdl:definitions name="Trading"
    xmlns:wsdl="http://schemas.xmlsoap.org/wsdl/"
    xmlns:soap="http://schemas.xmlsoap.org/wsdl/soap/"
    xmlns:xs="http://www.w3.org/2001/XMLSchema"
    xmlns:ns="urn:trading"
    targetNamespace="urn:trading">
  <wsdl:types>
    <xs:schema targetNamespace="urn:trading" elementFormDefault="qualified">
      <xs:complexType name="ItemType">
        <xs:sequence>
          <xs:element name="ItemID" type="xs:string" minOccurs="0"/>
          <xs:element name="Title" type="xs:string" minOccurs="0"/>
        </xs:sequence>
      </xs:complexType>
      <xs:element name="AddFixedPriceItemRequest">
        <xs:complexType><xs:sequence><xs:element name="Item" type="ns:ItemType"/></xs:sequence></xs:complexType>
      </xs:element>
      <xs:element name="AddFixedPriceItemResponse">
        <xs:complexType><xs:sequence>
          <xs:element name="Ack" type="xs:string"/>
          <xs:element name="ItemID" type="xs:string" minOccurs="0"/>
        </xs:sequence></xs:complexType>
      </xs:element>
      <xs:element name="GetItemRequest">
        <xs:complexType><xs:sequence><xs:element name="ItemID" type="xs:string" minOccurs="0"/></xs:sequence></xs:complexType>
      </xs:element>
      <xs:element name="GetItemResponse">
        <xs:complexType><xs:sequence>
          <xs:element name="Ack" type="xs:string"/>
          <xs:element name="Item" type="ns:ItemType" minOccurs="0"/>
        </xs:sequence></xs:complexType>
      </xs:element>
      <xs:element name="ReviseFixedPriceItemRequest">
        <xs:complexType><xs:sequence><xs:element name="Item" type="ns:ItemType"/></xs:sequence></xs:complexType>
      </xs:element>
      <xs:element name="ReviseFixedPriceItemResponse">
        <xs:complexType><xs:sequence><xs:element name="ItemID" type="xs:string" minOccurs="0"/></xs:sequence></xs:complexType>
      </xs:element>
    </xs:schema>
  </wsdl:types>
  <wsdl:message name="AddFixedPriceItemRequest"><wsdl:part name="body" element="ns:AddFixedPriceItemRequest"/></wsdl:message>
  <wsdl:message name="AddFixedPriceItemResponse"><wsdl:part name="body" element="ns:AddFixedPriceItemResponse"/></wsdl:message>
  <wsdl:message name="GetItemRequest"><wsdl:part name="body" element="ns:GetItemRequest"/></wsdl:message>
  <wsdl:message name="GetItemResponse"><wsdl:part name="body" element="ns:GetItemResponse"/></wsdl:message>
  <wsdl:message name="ReviseFixedPriceItemRequest"><wsdl:part name="body" element="ns:ReviseFixedPriceItemRequest"/></wsdl:message>
  <wsdl:message name="ReviseFixedPriceItemResponse"><wsdl:part name="body" element="ns:ReviseFixedPriceItemResponse"/></wsdl:message>
  <wsdl:portType name="TradingPort">
    <wsdl:operation name="AddFixedPriceItem">
      <wsdl:input message="ns:AddFixedPriceItemRequest"/><wsdl:output message="ns:AddFixedPriceItemResponse"/>
    </wsdl:operation>
    <wsdl:operation name="GetItem">
      <wsdl:input message="ns:GetItemRequest"/><wsdl:output message="ns:GetItemResponse"/>
    </wsdl:operation>
    <wsdl:operation name="ReviseFixedPriceItem">
      <wsdl:input message="ns:ReviseFixedPriceItemRequest"/><wsdl:output message="ns:ReviseFixedPriceItemResponse"/>
    </wsdl:operation>
  </wsdl:portType>
  <wsdl:binding name="TradingBinding" type="ns:TradingPort">
    <soap:binding style="document" transport="http://schemas.xmlsoap.org/soap/http"/>
    <wsdl:operation name="AddFixedPriceItem"><soap:operation soapAction="AddFixedPriceItem"/>
      <wsdl:input><soap:body use="literal"/></wsdl:input><wsdl:output><soap:body use="literal"/></wsdl:output></wsdl:operation>
    <wsdl:operation name="GetItem"><soap:operation soapAction="GetItem"/>
      <wsdl:input><soap:body use="literal"/></wsdl:input><wsdl:output><soap:body use="literal"/></wsdl:output></wsdl:operation>
    <wsdl:operation name="ReviseFixedPriceItem"><soap:operation soapAction="ReviseFixedPriceItem"/>
      <wsdl:input><soap:body use="literal"/></wsdl:input><wsdl:output><soap:body use="literal"/></wsdl:output></wsdl:operation>
  </wsdl:binding>
  <wsdl:service name="TradingService">
    <wsdl:port name="TradingPort" binding="ns:TradingBinding"><soap:address location="https://api.example.com/ws/api.dll"/></wsdl:port>
  </wsdl:service>
</wsdl:definitions>
"#;

#[test]
fn a_soap_record_named_for_its_type_is_read_by_its_type_key() {
    let b = compile(TRADING.as_bytes()).unwrap();
    let get = op(&b, "GetItem");
    assert_eq!(get.kind, OperationKind::Query);
    assert_eq!(get.resource_type.as_deref(), Some("item"));
    assert_eq!(
        get.identity,
        identity(
            "ItemID",
            Some("ItemID"),
            Some(RecordAt::Member("Item".into()))
        )
    );
    // The create answers an acknowledgement (`{Ack, ItemID}`), not the record it sends,
    // and nothing names the sent `Item` as the record: it keeps its name's type and no
    // identity, so the acknowledgement is never stored as an item.
    let add = op(&b, "AddFixedPriceItem");
    assert_eq!(add.kind, OperationKind::Command(Effect::Create));
    assert_eq!(add.resource_type.as_deref(), Some("fixedpriceitem"));
    assert_eq!(add.identity, None);
    // A name with no verb stays what it was: no write is guessed.
    let revise = op(&b, "ReviseFixedPriceItem");
    assert_eq!(revise.kind, OperationKind::Query);
    assert_eq!(
        revise.resource_type.as_deref(),
        Some("revisefixedpriceitem")
    );
    assert_eq!(revise.identity, None);

    let world = tier0(&b);
    assert_eq!(world.operations["GetItem"].kind, Some(OpKind::Read));
    assert_eq!(
        world.types["item"]
            .identity
            .as_ref()
            .and_then(|i| i.member.as_deref()),
        Some("ItemID")
    );
}

/// A create named for its type (`AddItem`) that answers an acknowledgement and the new
/// key (`{Ack, ItemID}`) is not keyed by that answer: the answer is not the record, so
/// it must never be stored as one.
#[test]
fn a_soap_create_answering_an_acknowledgement_keeps_no_identity() {
    let wsdl = TRADING.replace("AddFixedPriceItem", "AddItem");
    let b = compile(wsdl.as_bytes()).unwrap();
    let add = op(&b, "AddItem");
    assert_eq!(add.kind, OperationKind::Command(Effect::Create));
    assert_eq!(add.resource_type.as_deref(), Some("item"));
    assert_eq!(add.identity, None);
}
