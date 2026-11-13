#!/usr/bin/env node
// B4 acceptance on AUTHORED data (D130: no seed, no demo data). Runs after `platform-stack.sh smoke` against the same stack:
//   scripts/local/platform-stack.sh b4        (PS_STATE = journey.json, PS_REPORT = b4-report.json, both set by the harness)
//   node scripts/local/platform-stack-b4.mjs --list | --dry-run [--only A7,L1] | [--only ...] [--skip-authoring]
// NEEDS scripts/local/ps-lib.mjs (api mint step gap ensureLayer addRow tplOp templateOps customerChat assertionFor, J who env finish ...).
//
// 1. AUTHORING, only through product routes (idempotent: every call tolerates "already there"):
//    sa   POST /api/layers/{id}/rows  operation (25, from the final seed rows), activity (9), context_space (13); POST /api/platform/templates/{layer}/tags
//         POST /api/platform/connector-templates demo_retail@2 (the smoke's ops + delivery.list/redirect, order.cancel, payment.list/refund, warranty, dispute) + .../publish
//    ta   POST /api/connections/systems (b4-retail prod, b4-retail-test env=test NOT a stub: demo-backends faults act on X-Demo-Env: test and must
//         reach real data), PUT .../credential, POST /api/jobs {template: newest tag} (a NEW job b4_lib: a job pins its tags at creation and no
//         route applies a newer tag to an existing job, see the gap printed), POST/PUT channel-bindings + identity-secret (web chat, stays env=test),
//         PUT /api/jobs/{job}/staff, PUT .../operations/{op}/routes/{env}, POST /api/jobs/{job}/procedures (13) + .../status CANARY, PUBLISHED
//    demo-backends own admin routes (not platform): POST /admin/reset, PUT /admin/faults, POST /admin/scenarios/{name}, GET /admin/records/{table}
// 2. ROWS: A7 A13 A16 W0 W1 W2 W3 W-A7 L1..L11, every one as a logged-in known customer (web: signed assertion; staff chat: customer picked) with
//    natural messages only, or as staff (A13). Everything is asserted from the API: case status/outcome, operation calls, handover items, csat,
//    demo-backends records, and the reply text (no id, no lookup-key ask, no host/url/code).
// A row whose precondition is missing prints "PRODUCT GAP: <code> - why" and FAILS. Exit 1 on any FAIL. Never targets prod: the API/edge URLs
// come from the harness and must be loopback.
import { mkdirSync, readFileSync, writeFileSync, existsSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { randomBytes } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import {
	api, mint, step, gap, ensureLayer, addRow, tplOp, templateOps, assertionFor, // exactly the ps-lib names agent-8a exports
	J, who, env, finish, results, gaps, ok, itemsOf, edge, demoAdmin, demoCustomers, llmGap, slug, note, assert, sleep, TEMPLATE, ISSUER,
} from './ps-lib.mjs';

// Authored content below = the final seed rows of the deleted v1..v17 platform deltas (git: feat/application-completion crates/agent-api/seed/platform),
// with knowledge_kinds emptied (no knowledge is bound by this script) and the base-procedure references of the spaces dropped.
const DATA = {"ops":{"case.comment":{"discriminator":null,"attrs":{"modality":"do","is_derive":false,"side_effect":"W","idempotency":"key","confirm_back":true,"verify_read":null,"builtin":false,"op_kind":"standard","guard_rules":[],"identity_level":"2","input_fields":["case_id","comment"],"output_fields":["status"]},"description":"Add a message to a case.","retail":"O","stages":["Support"],"extra_errors":[]},"case.create{complaint}":{"discriminator":{"field":"case_type","value":"complaint"},"attrs":{},"description":"Complaint intake.","retail":"R","stages":["Support"],"extra_errors":[]},"case.create{dispute}":{"discriminator":{"field":"case_type","value":"dispute"},"attrs":{"input_fields":["subject_object","amount","description","customer_email"]},"description":"Dispute a charge.","retail":"R","stages":["Support"],"extra_errors":[]},"case.create{handover}":{"discriminator":{"field":"case_type","value":"handover"},"attrs":{"identity_level":"1","idempotency":"key","input_fields":["subject","transcript_ref","customer_email"]},"description":"Hand over to a person: the record handover leaves behind, in the agent_cases queue.","retail":"R","stages":["Support"],"extra_errors":["outside_hours"]},"case.create{return}":{"discriminator":{"field":"case_type","value":"return"},"attrs":{"input_fields":["subject_object","return_lines","return_method","refund_type","customer_email"],"output_fields":["case_id","status","label","deadline"],"guard_rules":["RET-01"]},"description":"Return / RMA; label as a Document.","retail":"R","stages":["Support"],"extra_errors":[]},"case.create{warranty_claim}":{"discriminator":{"field":"case_type","value":"warranty_claim"},"attrs":{"input_fields":["subject_object","description","photo_ref","customer_email"],"guard_rules":["WAR-01"]},"description":"Warranty claim on an owned unit.","retail":"R","stages":["Support"],"extra_errors":[]},"case.read":{"discriminator":null,"attrs":{"modality":"observe","is_derive":false,"side_effect":"R","idempotency":"nat","confirm_back":false,"verify_read":null,"builtin":false,"op_kind":"standard","guard_rules":[],"identity_level":"2","input_fields":["case_id"],"output_fields":["status","case_type","due_at","queue","title","latest_update"]},"description":"Status of a case. Default route agent_cases (K49). Also returns title (the ticket's subject) and latest_update (the ticket's last note to the requester, in plain words); neither carries an id.","retail":"R","stages":["Support"],"extra_errors":[]},"delivery.list":{"discriminator":null,"attrs":{"modality":"observe","is_derive":false,"side_effect":"R","idempotency":"nat","confirm_back":false,"verify_read":null,"builtin":false,"op_kind":"standard","guard_rules":[],"identity_level":"2","input_fields":["party_id"],"output_fields":["status","carrier","tracking_ref","tracking_url","expected_window","address","events","order_id"]},"description":"The customer's deliveries, one per parcel, each naming its order.","retail":"R","stages":["Delivery"],"extra_errors":[]},"delivery.read":{"discriminator":null,"attrs":{"modality":"observe","is_derive":false,"side_effect":"R","idempotency":"nat","confirm_back":false,"verify_read":null,"builtin":false,"op_kind":"standard","guard_rules":[],"identity_level":"2","input_fields":["order_id"],"output_fields":["status","carrier","tracking_ref","tracking_url","expected_window","address","events"]},"description":"Where is my parcel.","retail":"R","stages":["Delivery"],"extra_errors":[]},"delivery.read{by_tracking_ref}":{"discriminator":{"field":"lookup","value":"by_tracking_ref"},"attrs":{"identity_level":"1","input_fields":["tracking_ref"],"output_fields":["status","carrier","expected_window","events"]},"description":"Tracking ref is a possession factor: level 1, and no address fields come back.","retail":"R","stages":["Delivery"],"extra_errors":[]},"delivery.redirect":{"discriminator":null,"attrs":{"modality":"do","is_derive":false,"side_effect":"W","idempotency":"key","confirm_back":true,"verify_read":"delivery.read","builtin":false,"op_kind":"standard","guard_rules":[],"identity_level":"2","input_fields":["delivery_id","address"],"output_fields":["status","address"]},"description":"Send the parcel elsewhere or to a pickup point.","retail":"O","stages":["Delivery"],"extra_errors":[]},"delivery.redirect{by_order}":{"discriminator":{"field":"lookup","value":"by_order"},"attrs":{"identity_level":"2","input_fields":["order_id","address"],"output_fields":["status","address"]},"description":"Redirect the parcel of an order the customer owns; the system resolves the delivery from the order.","retail":"O","stages":["Delivery"],"extra_errors":[]},"delivery.report_problem":{"discriminator":null,"attrs":{"modality":"request","is_derive":false,"side_effect":"W","idempotency":"key+dup","confirm_back":true,"verify_read":null,"builtin":false,"op_kind":"standard","guard_rules":[],"identity_level":"2","input_fields":["delivery_id","problem","photo_ref"],"output_fields":["case_id","status"]},"description":"Damaged, missing or late parcel: opens a case at the carrier or helpdesk.","retail":"O","stages":["Delivery"],"extra_errors":[]},"delivery.report_problem{by_order}":{"discriminator":{"field":"lookup","value":"by_order"},"attrs":{"identity_level":"2","input_fields":["order_id","problem","photo_ref"],"output_fields":["case_id","status"]},"description":"Report a problem with the parcel of an order the customer owns; the system resolves the delivery.","retail":"O","stages":["Delivery"],"extra_errors":[]},"knowledge.search":{"discriminator":null,"attrs":{"modality":"observe","is_derive":false,"side_effect":"R","idempotency":"nat","confirm_back":false,"verify_read":null,"builtin":false,"op_kind":"standard","guard_rules":[],"identity_level":"0","input_fields":["query","kind","store"],"output_fields":["passages","citations"]},"description":"Passages + citations from strata_rag /api/query (K49).","retail":"R","stages":["Orientation","Investigation","Purchase","Delivery","Change","Support","Exit"],"extra_errors":[]},"order.cancel":{"discriminator":null,"attrs":{"modality":"do","is_derive":false,"side_effect":"X","idempotency":"key+dup","confirm_back":true,"verify_read":"order.read","builtin":false,"op_kind":"standard","guard_rules":[],"identity_level":"2","input_fields":["order_id","reason"],"output_fields":["status","refund_expected"]},"description":"Cancel an order. Only from quoted / processing / payment_due; every other status is an invalid_state error.","retail":"O","stages":["Change"],"extra_errors":[]},"order.read":{"discriminator":null,"attrs":{"modality":"observe","is_derive":false,"side_effect":"R","idempotency":"nat","confirm_back":false,"verify_read":null,"builtin":false,"op_kind":"standard","guard_rules":[],"identity_level":"2","input_fields":["order_id"],"output_fields":["status","lines","total","delivery_type","payment_method_ref","customer_id","customer_email","delivery_postcode"]},"description":"One order; also the verify_read for modify and cancel.","retail":"R","stages":["Purchase","Delivery","Change"],"extra_errors":[]},"party.list":{"discriminator":null,"attrs":{"modality":"observe","is_derive":false,"side_effect":"R","idempotency":"nat","confirm_back":false,"verify_read":null,"builtin":false,"op_kind":"standard","guard_rules":[],"identity_level":"1","input_fields":["email","phone","order_id"],"output_fields":["party_id","roles"]},"description":"Staff-only: a colleague can find the party by an identifier (email, phone or order number); never asked of a known customer. Names come back masked.","retail":"R","stages":["Purchase","Delivery","Change","Support","Exit"],"extra_errors":[]},"payment.create":{"discriminator":null,"attrs":{"modality":"do","is_derive":false,"side_effect":"P","idempotency":"key+dup","confirm_back":true,"verify_read":"payment.list","builtin":false,"op_kind":"hosted_payment_link","guard_rules":[],"identity_level":"2","input_fields":["order_id","amount"],"output_fields":["payment_url","expires_at"]},"description":"Hosted payment link only (D27): the reply carries the PSP link, never card data.","retail":"O","stages":["Purchase"],"extra_errors":[]},"payment.list":{"discriminator":null,"attrs":{"modality":"observe","is_derive":false,"side_effect":"R","idempotency":"nat","confirm_back":false,"verify_read":null,"builtin":false,"op_kind":"standard","guard_rules":[],"identity_level":"2","input_fields":["party_id","order_id"],"output_fields":["payment_id","direction","amount","method_ref","status","date","customer_id","customer_email","delivery_postcode","order_id"]},"description":"Payments and refunds; masked method only.","retail":"R","stages":["Purchase","Support"],"extra_errors":[]},"payment.refund":{"discriminator":null,"attrs":{"modality":"do","is_derive":false,"side_effect":"X","idempotency":"key+dup","confirm_back":true,"verify_read":"payment.list","builtin":false,"op_kind":"standard","guard_rules":[],"identity_level":"2","input_fields":["payment_id","amount","refund_reason"],"output_fields":["status","amount","expected_at"]},"description":"Refund to the original method; no card data.","retail":"O","stages":["Support"],"extra_errors":[]},"return.list":{"discriminator":null,"attrs":{"modality":"observe","is_derive":false,"side_effect":"R","idempotency":"nat","confirm_back":false,"verify_read":null,"builtin":false,"op_kind":"standard","guard_rules":[],"identity_level":"2","input_fields":["party_id"],"output_fields":["return_id","order_id","status","created_at"]},"description":"Lists the known customer's own returns, one per return, each naming its order and status (open, pending_provider, refunded or rejected); never asked by id.","retail":"R","stages":["Support"],"extra_errors":[]},"rule.evaluate{PAY-04}":{"discriminator":{"field":"rule_id","value":"PAY-04"},"attrs":{"input_fields":["payment_id"]},"description":"Pending-charge rule: a pending charge cannot be refunded yet.","retail":"R","stages":["Purchase","Support"],"extra_errors":[]},"rule.evaluate{RET-01}":{"discriminator":{"field":"rule_id","value":"RET-01"},"attrs":{"input_fields":["order_id","delivered_at","return_lines"]},"description":"Return window 30 days (shaped on schema.org MerchantReturnPolicy).","retail":"R","stages":["Support"],"extra_errors":[]},"rule.evaluate{WAR-01}":{"discriminator":{"field":"rule_id","value":"WAR-01"},"attrs":{"input_fields":["order_id","offering_id","purchase_date"]},"description":"Warranty 24 months.","retail":"R","stages":["Support"],"extra_errors":[]}},"activities":{"delivery:where_is_my_order":{"layer":"industry_dept/retail/customer_service","fields":{"label":"Where is my order","summary":"customer \u00b7 order.read + delivery.read","actor":"customer","operations":[{"op":"delivery.read","variant":"by_tracking_ref","required":true},{"op":"order.read","required":true},{"op":"knowledge.search","required":true},{"op":"delivery.read","required":true},{"op":"delivery.list","required":false}],"required":true,"objects":[{"object":"delivery","role":"subject","fields":["carrier","tracking_ref","tracking_url","status","expected_window","events"],"required_inputs":[]},{"object":"order","role":"context","fields":["order_id","status","lines"],"required_inputs":[]}],"trigger":"\u201cwhere is my parcel?\u201d","description":"Find out where my order is"}},"delivery:report_problem":{"layer":"industry_dept/retail/customer_service","fields":{"label":"Report a delivery problem","summary":"customer \u00b7 delivery.report_problem","actor":"customer","operations":[{"op":"party.list","required":true},{"op":"order.read","required":true},{"op":"delivery.read","required":true},{"op":"delivery.redirect","required":false},{"op":"delivery.report_problem","required":false},{"op":"case.create","variant":"handover","required":false}],"objects":[{"object":"delivery","role":"subject","fields":["delivery_id","carrier","status","events"],"required_inputs":[]},{"object":"order","role":"context","fields":["order_id","lines"],"required_inputs":[]}],"trigger":"\u201carrived damaged\u201d, \u201cstill not here\u201d","description":"Report a parcel that is damaged, wrong or missing"}},"change:cancel_order":{"layer":"industry_dept/retail/customer_service","fields":{"label":"Cancel an order","summary":"customer \u00b7 order.cancel (only quoted/processing/payment_due)","actor":"customer","operations":[{"op":"party.list","required":true},{"op":"order.read","required":true},{"op":"order.cancel","required":false}],"objects":[{"object":"order","role":"subject","fields":["order_id","status","lines"],"required_inputs":[]}],"trigger":"\u201ccancel my order\u201d","description":"Cancel an order that has not shipped yet"}},"support:return":{"layer":"industry_dept/retail/customer_service","fields":{"label":"Return an item","summary":"customer \u00b7 rule.evaluate{RET-01} + case.create{return}","actor":"customer","operations":[{"op":"party.list","required":true},{"op":"order.read","required":true},{"op":"rule.evaluate","variant":"RET-01","required":true},{"op":"case.create","variant":"return","required":true},{"op":"payment.list","required":true},{"op":"knowledge.search","required":true},{"op":"return.list","required":false}],"required":true,"objects":[{"object":"case{return}","role":"subject","fields":["case_id","status","return_lines","refund_type","deadline","label"],"required_inputs":["return_lines"]},{"object":"order","role":"context","fields":["order_id","placed_at","delivered_at","lines"],"required_inputs":[]},{"object":"rule","role":"context","fields":["rule_id","outcome","reason"],"required_inputs":[]}],"trigger":"\u201cI want to return this\u201d","description":"Send an item back"}},"support:refund_status":{"layer":"industry_dept/retail/customer_service","fields":{"label":"Where is my refund","summary":"customer \u00b7 payment.list + rule.evaluate{PAY-04}","actor":"customer","operations":[{"op":"party.list","required":true},{"op":"payment.list","required":true},{"op":"case.read","required":true},{"op":"payment.refund","required":false},{"op":"order.read","required":false},{"op":"rule.evaluate","variant":"RET-01","required":false},{"op":"rule.evaluate","variant":"PAY-04","required":false},{"op":"return.list","required":false}],"objects":[{"object":"payment","role":"subject","fields":["amount","status","eta","original_payment_id","direction"],"required_inputs":[]},{"object":"case","role":"context","fields":["case_id","status"],"required_inputs":[]}],"trigger":"\u201cwhere is my refund?\u201d","description":"Find out where my refund is"}},"purchase:payment_failed":{"layer":"base_dept/customer_service","fields":{"label":"Payment failed","summary":"customer \u00b7 payment.list + payment.create (hosted link only, D27)","actor":"customer","operations":[{"op":"party.list","required":true},{"op":"order.read","required":true},{"op":"payment.list","required":true},{"op":"rule.evaluate","variant":"PAY-04","required":true},{"op":"payment.create","required":false},{"op":"case.create","variant":"dispute","required":false}],"objects":[{"object":"order","role":"context","fields":["order_id","status","total"],"required_inputs":[]},{"object":"payment","role":"subject","fields":["payment_id","amount","status","method_masked","at"],"required_inputs":[]},{"object":"rule","role":"context","fields":["rule_id","outcome","reason"],"required_inputs":[]}],"trigger":"\u201cpayment failed\u201d, \u201ccard declined\u201d","description":"Find out why my payment did not go through and pay again"}},"support:warranty_claim":{"layer":"industry_dept/retail/customer_service","fields":{"label":"Warranty claim","summary":"customer \u00b7 rule.evaluate{WAR-01} + case.create{warranty_claim}","actor":"customer","operations":[{"op":"party.list","required":true},{"op":"order.read","required":true},{"op":"rule.evaluate","variant":"WAR-01","required":true},{"op":"case.create","variant":"warranty_claim","required":true},{"op":"case.read","required":true},{"op":"knowledge.search","required":true}],"objects":[{"object":"case{warranty_claim}","role":"subject","fields":["case_id","status","reason"],"required_inputs":["reason"]},{"object":"asset","role":"context","fields":["serial","warranty_until"],"required_inputs":[]},{"object":"rule","role":"context","fields":["rule_id","outcome","reason"],"required_inputs":[]}],"trigger":"\u201cbroken after 3 months\u201d","description":"Claim under warranty for something that broke"}},"support:complaint_intake":{"layer":"base","fields":{"label":"Take a complaint","summary":"person \u00b7 case.create{complaint}","actor":"customer","operations":[{"op":"case.create","variant":"complaint","required":true},{"op":"case.create","variant":"handover","required":true},{"op":"case.comment","required":false}],"objects":[{"object":"case{complaint}","role":"subject","fields":["case_id","status","reason"],"required_inputs":["reason"]},{"object":"order","role":"context","fields":["order_id","status"],"required_inputs":[]}],"trigger":"GC complaint about an outcome (late, wrong, damaged)","description":"Make a complaint and have it recorded"}},"support:data_rights":{"layer":"base_dept/customer_service","fields":{"label":"Data rights request","summary":"customer \u00b7 case.create{handover} to the privacy team","actor":"customer","operations":[{"op":"case.create","variant":"handover","required":true}],"objects":[{"object":"case{handover}","role":"subject","fields":["case_id","reason","priority"],"required_inputs":[]}],"trigger":"\u201cwhat data do you hold on me\u201d, \u201cdelete my data\u201d","description":"Ask what data you hold about me, or have it corrected or removed"}}},"spaces":{"retail:order-support:order_status":{"layer":"industry_dept/retail/customer_service","fields":{"label":"Order status","description":"The customer asks where an order is or when it arrives, without reporting a problem. (order_id, payment_id and party_id are resolved from the customer's own data, never asked.)","area":"order-support","knowledge_kinds":[],"slots":["order_id"],"activity":"delivery:where_is_my_order"}},"retail:logistics:track_parcel":{"layer":"industry_dept/retail/customer_service","fields":{"label":"Where is my parcel","description":"The customer wants the carrier's tracking status of a parcel that has shipped, found among their own deliveries, without reporting a problem.","area":"logistics","knowledge_kinds":[],"slots":[],"activity":"delivery:where_is_my_order"}},"retail:logistics:delivery_problem":{"layer":"industry_dept/retail/customer_service","fields":{"label":"Delivery problem","description":"The parcel is late, lost or damaged, or tracking says delivered but the customer did not receive it. (order_id, payment_id and party_id are resolved from the customer's own data, never asked.)","area":"logistics","knowledge_kinds":[],"slots":["order_id","problem"],"activity":"delivery:report_problem"}},"retail:logistics:redirect":{"layer":"industry_dept/retail/customer_service","fields":{"label":"Redirect a parcel","area":"logistics","knowledge_kinds":[],"description":"The customer wants a parcel that is on its way sent to another address. (order_id, payment_id and party_id are resolved from the customer's own data, never asked.)","slots":["order_id","address"],"activity":"delivery:report_problem"}},"retail:order-support:cancel_order":{"layer":"industry_dept/retail/customer_service","fields":{"label":"Cancel an order","area":"order-support","knowledge_kinds":[],"description":"The customer wants to cancel an order before it is dispatched. (order_id, payment_id and party_id are resolved from the customer's own data, never asked.)","slots":["order_id","reason"],"activity":"change:cancel_order"}},"retail:order-support:returns":{"layer":"industry_dept/retail/customer_service","fields":{"label":"Returns","description":"The customer wants to send back an item they received, or asks how to return it. (order_id, payment_id and party_id are resolved from the customer's own data, never asked.)","area":"order-support","knowledge_kinds":[],"slots":["order_id","reason"],"activity":"support:return"}},"retail:payments:refunds":{"layer":"industry_dept/retail/customer_service","fields":{"label":"Refunds","description":"The customer asks about money owed back to them: a refund that is missing, late or the wrong amount. (order_id, payment_id and party_id are resolved from the customer's own data, never asked.)","area":"payments","knowledge_kinds":[],"slots":["order_id"],"activity":"support:refund_status"}},"retail:payments:refund_request":{"layer":"industry_dept/retail/customer_service","fields":{"label":"Refund request","area":"payments","knowledge_kinds":[],"description":"The customer asks for money back on a cancelled or returned order. Asking where a refund is belongs to the read-only refunds space. (order_id, payment_id and party_id are resolved from the customer's own data, never asked.)","slots":["order_id","payment_id","reason"],"activity":"support:refund_status"}},"retail:payments:payment_errors":{"layer":"industry_dept/retail/customer_service","fields":{"label":"Payment error","area":"payments","knowledge_kinds":[],"description":"The customer was charged twice or a payment was declined or went wrong. (order_id, payment_id and party_id are resolved from the customer's own data, never asked.)","slots":["order_id"],"activity":"purchase:payment_failed"}},"retail:payments:pending_charge":{"layer":"industry_dept/retail/customer_service","fields":{"label":"Pending charge","description":"The customer sees a charge or a pending authorisation on their card or account that they do not recognise or that has not cleared. (order_id, payment_id and party_id are resolved from the customer's own data, never asked.)","area":"payments","knowledge_kinds":[],"slots":["order_id"],"activity":"purchase:payment_failed"}},"retail:order-support:warranty":{"layer":"industry_dept/retail/customer_service","fields":{"label":"Warranty","description":"An item the customer has used is faulty or broke, and they want it repaired or replaced under warranty. (order_id, payment_id and party_id are resolved from the customer's own data, never asked.)","area":"order-support","knowledge_kinds":[],"slots":["order_id","defect"],"activity":"support:warranty_claim"}},"customer_service:complaints:complaint_intake":{"layer":"base_dept/customer_service","fields":{"label":"Complaint intake","description":"The person is unhappy with the service or a decision and wants it recorded and put right.","area":"complaints","entities":["Party","Case"],"knowledge_kinds":[],"slots":["complaint_summary","desired_outcome"],"activity":"support:complaint_intake"}},"customer_service:privacy:data_rights":{"layer":"base_dept/customer_service","fields":{"label":"Data rights request","description":"The person asks to see, correct, export or delete the data held about them.","area":"privacy","entities":["Party","Consent"],"knowledge_kinds":[],"slots":["subject"],"activity":"support:data_rights"}}},"procedures":[{"name":"order_status","version":2,"context_space":"retail:order-support:order_status","description":"Read the customer's order and its delivery, then say where it is and when it arrives.","steps":[{"step_number":1,"phase":0,"name":"order","operation":"order.read","variant":null,"modality":"observe","side_effect":"R","identity_level":"2","confirm_back":false,"gate":false,"args":{"order_id":"$order_id"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null},{"step_number":2,"phase":1,"name":"delivery","operation":"delivery.read","variant":null,"modality":"observe","side_effect":"R","identity_level":"2","confirm_back":false,"gate":false,"args":{"order_id":"$order_id"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null}],"tool_requirements":["order.read","delivery.read"]},{"name":"track_parcel","version":1,"context_space":"retail:logistics:track_parcel","description":"List the known customer's parcels and say where each one is; the customer is never asked for a tracking number.","steps":[{"step_number":1,"phase":0,"name":"delivery","operation":"delivery.list","variant":null,"modality":"observe","side_effect":"R","identity_level":"2","confirm_back":false,"gate":false,"args":{"party_id":"$party_id"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null}],"tool_requirements":["delivery.list"]},{"name":"delivery_problem","version":2,"context_space":"retail:logistics:delivery_problem","description":"Log the delivery problem against the order's parcel, then pass it to a colleague.","steps":[{"step_number":1,"phase":0,"name":"order","operation":"order.read","variant":null,"modality":"observe","side_effect":"R","identity_level":"2","confirm_back":false,"gate":false,"args":{"order_id":"$order_id"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null},{"step_number":2,"phase":1,"name":"delivery","operation":"delivery.read","variant":null,"modality":"observe","side_effect":"R","identity_level":"2","confirm_back":false,"gate":false,"args":{"order_id":"$order_id"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null},{"step_number":3,"phase":2,"name":"report","operation":"delivery.report_problem","variant":"by_order","modality":"request","side_effect":"W","identity_level":"2","confirm_back":true,"gate":false,"args":{"order_id":"$order_id","problem":"$problem","photo_ref":"$photo_ref"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null},{"step_number":4,"phase":3,"name":"handover","operation":"case.create","variant":"handover","modality":"request","side_effect":"W","identity_level":"1","confirm_back":true,"gate":false,"args":{"subject":"$problem"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null}],"tool_requirements":["order.read","delivery.read","delivery.report_problem{by_order}","case.create{handover}"]},{"name":"redirect_delivery","version":1,"context_space":"retail:logistics:redirect","description":"Redirect the order's parcel to a new address while it is still on its way.","steps":[{"step_number":1,"phase":0,"name":"order","operation":"order.read","variant":null,"modality":"observe","side_effect":"R","identity_level":"2","confirm_back":false,"gate":false,"args":{"order_id":"$order_id"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null},{"step_number":2,"phase":1,"name":"delivery","operation":"delivery.read","variant":null,"modality":"observe","side_effect":"R","identity_level":"2","confirm_back":false,"gate":false,"args":{"order_id":"$order_id"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null},{"step_number":3,"phase":2,"name":"redirect","operation":"delivery.redirect","variant":"by_order","modality":"do","side_effect":"W","identity_level":"2","confirm_back":true,"gate":true,"args":{"order_id":"$order_id","address":"$address"},"preconditions":[{"id":"on_track","fact":"steps.delivery.out.status","op":"in","value":["created","ready","in_transit","pickup_available"]}],"postconditions":[],"performed_by":null,"max_per_case":null}],"tool_requirements":["order.read","delivery.read","delivery.redirect{by_order}"]},{"name":"cancel_order","version":1,"context_space":"retail:order-support:cancel_order","description":"Cancel an order that has not been dispatched; a dispatched order is returned instead.","steps":[{"step_number":1,"phase":0,"name":"order","operation":"order.read","variant":null,"modality":"observe","side_effect":"R","identity_level":"2","confirm_back":false,"gate":false,"args":{"order_id":"$order_id"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null},{"step_number":2,"phase":1,"name":"cancel","operation":"order.cancel","variant":null,"modality":"do","side_effect":"X","identity_level":"2","confirm_back":true,"gate":true,"args":{"order_id":"$order_id","reason":"$reason"},"preconditions":[{"id":"cancellable","fact":"steps.order.out.status","op":"in","value":["quoted","payment_due","processing"]}],"postconditions":[],"performed_by":null,"max_per_case":1}],"tool_requirements":["order.read","order.cancel"]},{"name":"return_request","version":3,"context_space":"retail:order-support:returns","description":"Check the 30-day return window (RET-01), then open the return case.","steps":[{"step_number":1,"phase":0,"name":"order","operation":"order.read","variant":null,"modality":"observe","side_effect":"R","identity_level":"2","confirm_back":false,"gate":false,"args":{"order_id":"$order_id"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null},{"step_number":2,"phase":1,"name":"rules","operation":"rule.evaluate","variant":"RET-01","modality":"observe","side_effect":"R","identity_level":"2","confirm_back":false,"gate":true,"args":{"order_id":"$order_id","delivered_at":"$order.delivered_at","return_lines":"$order.lines"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null},{"step_number":3,"phase":2,"name":"rma","operation":"case.create","variant":"return","modality":"request","side_effect":"W","identity_level":"2","confirm_back":true,"gate":false,"args":{"subject_object":"$order_id","return_lines":"$order.lines","return_method":"$return_method","refund_type":"$refund_type"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null}],"tool_requirements":["order.read","rule.evaluate{RET-01}","case.create{return}"]},{"name":"refund_status","version":2,"context_space":"retail:payments:refunds","description":"Read-only: say what happened to the money on the order. It never issues a refund.","steps":[{"step_number":1,"phase":0,"name":"payments","operation":"payment.list","variant":null,"modality":"observe","side_effect":"R","identity_level":"2","confirm_back":false,"gate":false,"args":{"order_id":"$order_id"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null},{"step_number":2,"phase":1,"name":"order","operation":"order.read","variant":null,"modality":"observe","side_effect":"R","identity_level":"2","confirm_back":false,"gate":false,"args":{"order_id":"$order_id"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null},{"step_number":3,"phase":2,"name":"rules","operation":"rule.evaluate","variant":"PAY-04","modality":"observe","side_effect":"R","identity_level":"2","confirm_back":false,"gate":false,"args":{"order_id":"$order_id"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null}],"tool_requirements":["payment.list","order.read","rule.evaluate{PAY-04}"]},{"name":"refund_request","version":1,"context_space":"retail:payments:refund_request","description":"Refund the order's own payment once, only when the order is cancelled or its return is complete and that payment (the first of payment.list's rows whose order_id is this order's) exists and is not already refunded; no payment of this order means no refund. Never offered in assistant mode.","steps":[{"step_number":1,"phase":0,"name":"payments","operation":"payment.list","variant":null,"modality":"observe","side_effect":"R","identity_level":"2","confirm_back":false,"gate":false,"args":{"order_id":"$order_id"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null},{"step_number":2,"phase":1,"name":"order","operation":"order.read","variant":null,"modality":"observe","side_effect":"R","identity_level":"2","confirm_back":false,"gate":false,"args":{"order_id":"$order_id"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null},{"step_number":3,"phase":2,"name":"refund","operation":"payment.refund","variant":null,"modality":"do","side_effect":"X","identity_level":"2","confirm_back":true,"gate":true,"args":{"order_id":"$order_id","payment_id":"$payments.rows[order_id=$order_id].payment_id","refund_reason":"$reason"},"preconditions":[{"id":"refundable","fact":"steps.order.out.status","op":"in","value":["cancelled","returned"]},{"id":"payment_found","fact":"steps.payments.out.rows[order_id=$order_id].payment_id","op":"exists"},{"id":"not_refunded","fact":"steps.payments.out.rows[order_id=$order_id].status","op":"ne","value":"refunded"}],"postconditions":[],"performed_by":null,"max_per_case":1}],"tool_requirements":["payment.list","order.read","payment.refund"]},{"name":"payment_problem_errors","version":1,"context_space":"retail:payments:payment_errors","description":"Check the order's payments (PAY-04) and open a dispute; never takes a new payment.","steps":[{"step_number":1,"phase":0,"name":"order","operation":"order.read","variant":null,"modality":"observe","side_effect":"R","identity_level":"2","confirm_back":false,"gate":false,"args":{"order_id":"$order_id"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null},{"step_number":2,"phase":1,"name":"payments","operation":"payment.list","variant":null,"modality":"observe","side_effect":"R","identity_level":"2","confirm_back":false,"gate":false,"args":{"order_id":"$order_id"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null},{"step_number":3,"phase":2,"name":"rules","operation":"rule.evaluate","variant":"PAY-04","modality":"observe","side_effect":"R","identity_level":"2","confirm_back":false,"gate":false,"args":{"order_id":"$order_id"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null},{"step_number":4,"phase":3,"name":"dispute","operation":"case.create","variant":"dispute","modality":"request","side_effect":"W","identity_level":"2","confirm_back":true,"gate":false,"args":{"subject_object":"$order_id"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null}],"tool_requirements":["order.read","payment.list","rule.evaluate{PAY-04}","case.create{dispute}"]},{"name":"payment_problem_pending","version":1,"context_space":"retail:payments:pending_charge","description":"Check the order's payments (PAY-04) and open a dispute; never takes a new payment.","steps":[{"step_number":1,"phase":0,"name":"order","operation":"order.read","variant":null,"modality":"observe","side_effect":"R","identity_level":"2","confirm_back":false,"gate":false,"args":{"order_id":"$order_id"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null},{"step_number":2,"phase":1,"name":"payments","operation":"payment.list","variant":null,"modality":"observe","side_effect":"R","identity_level":"2","confirm_back":false,"gate":false,"args":{"order_id":"$order_id"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null},{"step_number":3,"phase":2,"name":"rules","operation":"rule.evaluate","variant":"PAY-04","modality":"observe","side_effect":"R","identity_level":"2","confirm_back":false,"gate":false,"args":{"order_id":"$order_id"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null},{"step_number":4,"phase":3,"name":"dispute","operation":"case.create","variant":"dispute","modality":"request","side_effect":"W","identity_level":"2","confirm_back":true,"gate":false,"args":{"subject_object":"$order_id"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null}],"tool_requirements":["order.read","payment.list","rule.evaluate{PAY-04}","case.create{dispute}"]},{"name":"warranty_claim","version":2,"context_space":"retail:order-support:warranty","description":"Check the warranty period (WAR-01), then open the claim.","steps":[{"step_number":1,"phase":0,"name":"order","operation":"order.read","variant":null,"modality":"observe","side_effect":"R","identity_level":"2","confirm_back":false,"gate":false,"args":{"order_id":"$order_id"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null},{"step_number":2,"phase":1,"name":"rules","operation":"rule.evaluate","variant":"WAR-01","modality":"observe","side_effect":"R","identity_level":"2","confirm_back":false,"gate":true,"args":{"order_id":"$order_id","purchase_date":"$order.placed_at"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null},{"step_number":3,"phase":2,"name":"claim","operation":"case.create","variant":"warranty_claim","modality":"request","side_effect":"W","identity_level":"2","confirm_back":true,"gate":false,"args":{"subject_object":"$order_id","description":"$defect","photo_ref":"$photo_ref"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null}],"tool_requirements":["order.read","rule.evaluate{WAR-01}","case.create{warranty_claim}"]},{"name":"complaint_intake","version":2,"context_space":"customer_service:complaints:complaint_intake","description":"Record the complaint as a case, then pass it to a colleague. No identity check: it names no account data.","steps":[{"step_number":1,"phase":0,"name":"complaint","operation":"case.create","variant":"complaint","modality":"request","side_effect":"W","identity_level":"2","confirm_back":true,"gate":false,"args":{"subject":"$complaint_summary","description":"$desired_outcome"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null},{"step_number":2,"phase":1,"name":"handover","operation":"case.create","variant":"handover","modality":"request","side_effect":"W","identity_level":"1","confirm_back":true,"gate":false,"args":{"subject":"$complaint_summary"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null}],"tool_requirements":["case.create{complaint}","case.create{handover}"]},{"name":"data_rights_request","version":1,"context_space":"customer_service:privacy:data_rights","description":"Pass the data rights request to the privacy team.","steps":[{"step_number":1,"phase":0,"name":"handover","operation":"case.create","variant":"handover","modality":"request","side_effect":"W","identity_level":"1","confirm_back":true,"gate":false,"args":{"subject":"$subject"},"preconditions":[],"postconditions":[],"performed_by":null,"max_per_case":null}],"tool_requirements":["case.create{handover}"]}]};

// ---- options ---------------------------------------------------------------------------------------------------------
// The connector lists payments by the bound party; authored steps also carry the selected order as a filter.
// These are new procedure versions because published tenant versions are immutable.
for (const p of DATA.procedures) {
 let changed = false;
 for (const step of p.steps) {
  if (step.operation === 'rule.evaluate' && step.variant === 'PAY-04') {
   step.args = { ...step.args, payment_id: '$payments.rows[order_id=$order_id].payment_id', 'payment.status': '$payments.rows[order_id=$order_id].status', 'payment.at': '$payments.rows[order_id=$order_id].date' };
   changed = true;
  }
  if (step.operation === 'case.create' && step.variant === 'dispute') {
   step.args = { ...step.args, description: '$subject', amount: '$payments.rows[order_id=$order_id].amount' };
   changed = true;
  }
  if (step.operation === 'payment.list') {
  step.args = { ...step.args, party_id: '$party_id' };
  changed = true;
  }
 }
 if (changed) p.version += 1;
}

// A dispute is described in the customer's own words: the payment spaces collect them as `subject`.
for (const k of ['retail:payments:payment_errors', 'retail:payments:pending_charge']) {
 const f = DATA.spaces[k].fields;
 if (!f.slots.includes('subject')) f.slots = [...f.slots, 'subject'];
}

const argv = process.argv.slice(2);
const flag = (n) => argv.includes(n);
const optv = (n) => (argv.includes(n) ? argv[argv.indexOf(n) + 1] : undefined);
const LIST = flag('--list');
const DRY = flag('--dry-run');
const SKIP_AUTHORING = flag('--skip-authoring');
const ONLY = (optv('--only') ?? process.env.PS_B4_ONLY ?? '').split(',').map((s) => s.trim().toUpperCase()).filter(Boolean);
const WAIT_MS = Number(process.env.PS_B4_WAIT_S ?? 90) * 1000;
const HERE = import.meta.dirname;
const REPO = resolve(HERE, '../..');
const MAIN = (() => {
	try {
		return dirname(execFileSync('git', ['-C', REPO, 'rev-parse', '--path-format=absolute', '--git-common-dir'], { encoding: 'utf8' }).trim());
	} catch {
		return REPO;
	}
})();
const REPORT = process.env.PS_REPORT ?? join(MAIN, 'target/local/platform-stack/b4-report.json');
const JOB_KEY = 'b4_lib_policies';
// b4's OWN demo-backends (platform-stack.sh b4 starts it and exports these); the shared :8197 is never touched
const B4_DEMO_URL = (process.env.PS_B4_DEMO_URL ?? '').replace(/\/$/, '');
const B4_DEMO_TOKEN = process.env.PS_B4_DEMO_TOKEN ?? '';
if (!B4_DEMO_URL || !B4_DEMO_TOKEN) { console.error('PS_B4_DEMO_URL / PS_B4_DEMO_TOKEN are required (run via platform-stack.sh b4)'); process.exit(2); }
const BINDING_LABEL = 'B4 web chat (test)';
const B4_TEMPLATE_VERSION = '7';
const SYS = { prod: `b4-retail-v${B4_TEMPLATE_VERSION}`, test: `b4-retail-v${B4_TEMPLATE_VERSION}-test` };
const BUILTIN = /^(knowledge\.(search|answer)|credential\.verify|rule\.evaluate)(\{.*\})?$/;
let JOB = null;
const jp = (s) => `/api/jobs/${JOB}${s}`;
const pgap = (code, why) => {
	console.log(`PRODUCT GAP: ${code} - ${why}`);
	return gap(code, why);
};
const isOk = (r) => r.status >= 200 && r.status < 300;
const short = (r) => `${r.status} ${String(r.text).slice(0, 300)}`;

// ---- the id guard (JS twin of contains_id_pattern) + lookup-key asks + leaks ------------------------------------------
const PREFIXED = /\b(RET|ORD|O|SHP|CS|PAY|RMA|REF|DP|WAR|C|DEL|TRK)-[0-9A-Z]/;
const GENERIC = /(?<![A-Za-z0-9])[A-Z]{2,5}-?\d{2,}(?![A-Za-z0-9])/;
const UUID = /\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/i;
const DEMO_IDS = [/(?<![A-Za-z0-9])(pat|acc|cust)-\d+/i, /[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)+/];
function containsId(text, allow = []) {
	let t = text;
	for (const a of allow) t = t.split(a).join(' ');
	if (PREFIXED.test(t) || GENERIC.test(t) || UUID.test(t) || DEMO_IDS.some((re) => re.test(t))) return true;
	for (const m of t.matchAll(/\d+/g)) {
		const idx = m.index ?? 0;
		const before = t.slice(Math.max(0, idx - 2), idx);
		const after = t.slice(idx + m[0].length, idx + m[0].length + 2);
		if (m[0].length >= 6 && !(/\d[.,]$/.test(before) || /^[.,]\d/.test(after))) return true;
	}
	return false;
}
const ASKS = [
	/(order|customer|account|user|reference|case) (number|no\b|id)\b/i,
	/post ?code|zip code/i,
	/e-?mail address/i,
	/(could|can|please|may) (you )?(give|tell|provide|share|send|confirm|enter|type)[^.?!]{0,60}\b(number|reference|id|email|address)\b/i,
];
const LEAK = [/https?:\/\//i, /\b[a-z0-9-]+\.(internal|local|test|example)\b/i, /\b[a-z]+_[a-z]+(_[a-z]+)*\b/, /\b(40[0-9]|50[0-9])\b/];

// ---- the authored operations the contract offers, mapped onto demo-backends' routes (extends the smoke's demo_retail@1) ---
function extraTemplateOps() {
	const ship = { id: 'SHP-1', order_id: 'RET-01', status: 'in_transit', eta: '2026-10-05T00:00:00Z', carrier: 'DHL', tracking: 'SHP-1', address: 'x, y' };
	const shipMaps = [['$.items[*].status', 'status'], ['$.items[*].carrier', 'carrier'], ['$.items[*].tracking', 'tracking_ref'], ['$.items[*].eta', 'expected_window'], ['$.items[*].address', 'address'], ['$.items[*].order_id', 'order_id']];
	const redirectBody = '{"address":"{{canonical.address}}"}';
	const redirectMaps = [['$.status', 'status'], ['$.address', 'address']];
	const redirected = { id: 'SHP-1', status: 'in_transit', address: 'x, y', redirected_at: '2026-10-05T00:00:00Z', tracking: 'SHP-1' };
	const pay = { party_id:'cust-1001', email:'emma@example.test', postcode:'3511 AV', payment_id: 'PAY-1', order_id: 'RET-01', direction: 'charge', amount: '49.00', method_ref: 'Visa', status: 'captured', date: '2026-10-01T00:00:00Z' };
	const caseMaps = [['$.case_id', 'case_id'], ['$.status', 'status'], ['$.due_at', 'due_at']];
	const caseBody = { case_id: 'CS-50101', case_type: 'dispute', status: 'open', due_at: '2026-10-28T00:00:00Z', queue: 'Payments', order_id: 'RET-01', subject: 'x' };
	return [
        { ...templateOps().find(o => o.object === 'order' && o.verb === 'read' && o.variant === '*'),
            field_map: [...templateOps().find(o => o.object === 'order' && o.verb === 'read' && o.variant === '*').field_map,
                { path: '$.placed_at', field: 'placed_at', transform: 'none' },
                { path: '$.delivered_at', field: 'delivered_at', transform: 'none' }],
            fixtures: templateOps().find(o => o.object === 'order' && o.verb === 'read' && o.variant === '*').fixtures.map(f => f.outcome === 'ok' ? { ...f, body: { ...f.body, placed_at: '2026-10-01', delivered_at: '2026-10-05' } } : f) },
		tplOp('delivery', 'read', '*', 'R', 'GET', '/shipments?order_id={{canonical.order_id}}', '', shipMaps.filter(([, field]) => field !== 'order_id').map(([path, field]) => [path.replace('[*]', '[0]'), field]), { items: [ship] }),
		tplOp('delivery', 'list', '*', 'R', 'GET', '/shipments?customer_id={{canonical.party_id}}', '', shipMaps, { items: [ship] }),
		tplOp('delivery', 'read', 'by_tracking_ref', 'R', 'GET', '/shipments?tracking_ref={{canonical.tracking_ref}}', '', [shipMaps[0], shipMaps[1], shipMaps[3]], { items: [ship] }),
		tplOp('delivery', 'redirect', '*', 'W', 'POST', '/shipments/{{canonical.delivery_id}}/redirect', redirectBody, redirectMaps, redirected),
		tplOp('delivery', 'redirect', 'by_order', 'W', 'POST', '/orders/{{canonical.order_id}}/shipment/redirect', redirectBody, redirectMaps, redirected),
		{ ...tplOp('order', 'cancel', '*', 'X', 'POST', '/orders/{{canonical.order_id}}/cancel', '{"reason":"{{canonical.reason}}"}', [['$.status', 'status'], ['$.refund_expected', 'refund_expected']], { order_id: 'RET-04', status: 'cancelled', previous_status: 'processing', refund_expected: true }), verify_read: 'order.read' },
		tplOp('payment', 'list', '*', 'R', 'GET', '/payments?customer_id={{canonical.party_id?}}&order_id={{canonical.order_id?}}', '', [['$.payments[*].payment_id', 'payment_id'], ['$.payments[*].direction', 'direction'], ['$.payments[*].amount', 'amount'], ['$.payments[*].method_ref', 'method_ref'], ['$.payments[*].status', 'status'], ['$.payments[*].date', 'date'], ['$.payments[*].order_id', 'order_id'], ['$.payments[*].party_id', 'customer_id'], ['$.payments[*].email', 'customer_email'], ['$.payments[*].postcode', 'delivery_postcode']], { payments: [pay] }),
		{ ...tplOp('payment', 'refund', '*', 'X', 'POST', '/refunds', '{"payment_id":"{{canonical.payment_id}}","reason":"{{canonical.refund_reason}}"}', [['$.status', 'status'], ['$.amount', 'amount'], ['$.expected_at', 'expected_at']], { refund_id: 'REF-1', payment_id: 'PAY-1', amount: '49.00', status: 'succeeded', expected_at: '2026-10-10' }), verify_read: 'payment.list' },
		tplOp('case', 'create', 'warranty_claim', 'W', 'POST', '/warranty_claims', '{"order_id":"{{canonical.subject_object}}","defect":"{{canonical.description}}","ref":"{{step.idempotency_key}}"}', caseMaps, { claim_id: 'WC-1', case_id: 'WC-1', status: 'submitted', due_at: '2026-10-28T00:00:00Z' }),
		tplOp('case', 'create', 'dispute', 'W', 'POST', '/cases', '{"type":"dispute","subject":"{{canonical.description}}","order_id":"{{canonical.subject_object}}","ref":"{{step.idempotency_key}}"}', caseMaps, caseBody),
	];
}

// ---- demo-backends (the tenant's external system) ---------------------------------------------------------------------
async function resetDemo() {
	const r = await demoAdmin('POST', '/admin/reset', { seed: 42 });
	assert(r.status === 200, `demo-backends POST /admin/reset -> ${short(r)}`);
}
async function setFault(operation, mode, count = 1) {
	const r = await demoAdmin('PUT', '/admin/faults', mode === 'none' ? { operation, mode } : { operation, mode, count });
	if (r.status !== 200) throw pgap('demo_fault_unavailable', `PUT /admin/faults ${operation}/${mode} -> ${short(r)}`);
}
async function clearFaults() {
	for (const op of ['payment.refund', 'payment.list']) await demoAdmin('PUT', '/admin/faults', { operation: op, mode: 'none' }).catch(() => {});
}
async function scenario(name) {
	const list = await demoAdmin('GET', '/admin/scenarios');
	if (list.status !== 200 || !JSON.stringify(list.json).includes(`"${name}"`)) throw pgap('demo_scenario_missing', `demo-backends offers no scenario ${name} (GET /admin/scenarios -> ${short(list)})`);
	const r = await demoAdmin('POST', `/admin/scenarios/${name}`);
	assert(r.status === 200, `scenario ${name} -> ${short(r)}`);
}
async function demoRows(table, query = '') {
	const r = await demoAdmin('GET', `/admin/records/${table}?limit=500&q=${encodeURIComponent(query)}`);
	assert(r.status === 200, `demo-backends records/${table} -> ${short(r)}`);
	return r.json?.rows ?? [];
}
// This is external order lifecycle data, prepared in b4's isolated backend only. Keep the product's refund
// precondition: a delivered order cannot be refunded until its return has completed.
async function refundableOrder(r) {
 const changed = await demoAdmin('PUT','/admin/records/orders/RET-01',{status:'returned'});
 assert(changed.status===200, `prepare external returned order: ${short(changed)}`);
 assert(await orderStatus('RET-01') === 'returned', 'external RET-01 is not returned');
 r.note('Precondition: the external system records the wool rug order as returned; its captured payment is refundable.');
}

async function refundCount(orderId) {
	const pays = (await demoRows('payments', orderId)).filter((p) => p.order_id === orderId).map((p) => p.payment_id);
 const refunds = await Promise.all(pays.map(async id => (await demoRows('refunds', id)).filter(x=>x.payment_id===id).length));
 return refunds.reduce((a,b)=>a+b,0);
}
// Observe the external write before breaking its verification read. Start before the first
// message: a staff confirmation can already be carried by the initial request.
function failRefundVerification(before) {
 let stopped = false;
 const done = (async () => {
  for (const end = Date.now() + WAIT_MS; !stopped && Date.now() < end; ) {
   if ((await refundCount('RET-01')) - before === 1) {
    await setFault('payment.list', 'error', 1);
    return true;
   }
   await sleep(100);
  }
  return false;
 })();
 // Keep an early monitoring failure handled until the caller awaits it.
 done.catch(() => {});
 return { done, stop: () => { stopped = true; } };
}
async function orderStatus(orderId) {
 const r=await demoAdmin('GET',`/admin/records/orders/${encodeURIComponent(orderId)}`);
 assert(r.status===200,`read external order ${orderId}: ${short(r)}`);
 return String(r.json?.status ?? 'unknown');
}

// ---- the platform stack's model: the keyword fake covers 4 intents only (scripts/local/kc-fake-llm.mjs) ------------------
let fakeLlm = null;
async function isFakeLlm() {
	if (fakeLlm !== null) return fakeLlm;
	try {
		const r = await fetch(`${env.LLM_URL.replace(/\/v1$/, '')}/__calls`);
		fakeLlm = r.status === 200;
	} catch {
		fakeLlm = false;
	}
	return fakeLlm;
}
// kc-fake-llm.mjs TAILS (1fa46933) selects every B4 library space by its key tail
const FAKE_SPACES = { has: (k) => /:(track_parcel|order_status|delivery_problem|redirect(_delivery)?|cancel_order|returns|refunds|refund_request|payment_errors|pending_charge|dispute|warranty|complaint(_intake)?|data_rights|handover_to_person|shipping_info)$/.test(k) };
async function needSpaces(spaces) {
	if (!(await isFakeLlm())) return;
	const missing = spaces.filter((s) => !FAKE_SPACES.has(s));
	if (missing.length) throw pgap('fake_llm_cannot_select', `the fake LLM (scripts/local/kc-fake-llm.mjs) only selects track_parcel, refunds, returns and handover_to_person; this row needs ${missing.join(', ')}. Run the stack with --real-llm`);
}

// ---- state shared by the rows ------------------------------------------------------------------------------------------
const P = {}; // personas by customer id
const OPEN = new Set(['open', 'identity_required', 'confirmation_required']);
const caseRow = async (caseId) => {
	const r = await api('ta', 'GET', jp(`/cases/${encodeURIComponent(caseId)}`));
	assert(isOk(r), `case ${caseId} -> ${short(r)}`);
	return r.json?.case ?? r.json;
};
const callsOf = async (caseId) => {
	const r = await api('ta', 'GET', jp(`/operation-calls?case_id=${encodeURIComponent(caseId)}`));
	assert(isOk(r), `operation-calls -> ${short(r)}`);
	return itemsOf(r.json);
};
const named = (calls, name) => calls.filter((c) => c.op === name || (!name.includes('{') && String(c.op).startsWith(`${name}{`)));
const handoverItems = async (caseId) => {
	const r = await api('ta', 'GET', jp('/handover?state=all'));
	assert(isOk(r), `handover queue -> ${short(r)}`);
	return itemsOf(r.json).filter((i) => i.case_ref === caseId);
};
async function settle(caseId, ms = WAIT_MS) {
	const end = Date.now() + ms;
	let steady = 0;
	let row = null;
	while (Date.now() < end) {
		row = await caseRow(caseId);
		steady = !OPEN.has(row.status) || row.alert ? steady + 1 : 0;
		if (steady >= 2) return row;
		await sleep(1000);
	}
	return row;
}

// staff test chat (env=test) as a picked known customer
async function staffChat(cust) {
	let s = await api('ta', 'POST', jp('/conversations'), { env: 'test', customer_email: cust.email });
	if (s.status >= 400 && s.status < 500) {
		const s2 = await api('ta', 'POST', jp('/conversations'), { env: 'test', customer_id: cust.customer_id });
		if (isOk(s2)) s = s2;
	}
	const g = llmGap(s);
	if (g) throw g;
	assert(isOk(s), `start staff test chat as ${cust.customer_id} -> ${short(s)}`);
	const id = s.json.conversation_id;
	const view = async () => {
		const c = await api('ta', 'GET', jp(`/conversations/${encodeURIComponent(id)}`));
		assert(isOk(c), `conversation read -> ${short(c)}`);
		const b = c.json;
		return { texts: (b.messages ?? []).filter((m) => m.role !== 'customer' && m.role !== 'user' && m.kind !== 'note').map((m) => String(m.text ?? '')), caseId: b.conversation?.case_id ?? null };
	};
	const post = async (text) => {
		for (let i = 0; i < 20; i++) {
			const r = await api('ta', 'POST', jp(`/conversations/${encodeURIComponent(id)}/messages`), { text });
			if (r.status === 202 || r.status === 200) return;
			if (r.status !== 409) throw new Error(`send -> ${short(r)}`);
			await sleep(1000); // conversation_busy
		}
		throw new Error('conversation stayed busy');
	};
	const heard = [];
	return {
		id,
		all: () => heard,
		sendNoWait: post,
		async send(text) {
			const before = (await view()).texts.length;
			await post(text);
			const end = Date.now() + WAIT_MS;
			let seen = before;
			let quiet = 0;
			while (Date.now() < end) {
				await sleep(1000);
				const n = (await view()).texts.length;
				if (n > seen) {
					seen = n;
					quiet = 0;
				} else if (n > before && ++quiet >= 2) break;
			}
			if (seen === before) throw new Error(`no reply to "${text}" within ${WAIT_MS} ms`);
			const fresh = (await view()).texts.slice(before);
			heard.push(...fresh);
			return fresh;
		},
		async caseId() {
			return (await view()).caseId;
		},
	};
}
async function waitCase(chat) {
	for (let i = 0; i < 20; i++) {
		const c = await chat.caseId();
		if (c) return c;
		await sleep(500);
	}
	return null;
}
// send `first`, say yes at every confirm-back, follow up until `until` holds or the case is over
async function drive(r, chat, first, o = {}) {
	const queue = [first, ...(o.follow ?? [])];
	let confirms = 0;
	let caseId = null;
	while (queue.length) {
		await chat.send(queue.shift());
		caseId = caseId ?? (await waitCase(chat));
		if (!caseId) {
            if (queue.length) continue;
            throw new Error('the conversation opened no case');
        }
		let row = await settle(caseId, 20_000);
		if (row.status === 'confirmation_required' && confirms < 3) {
			confirms++;
			await chat.send('Yes, please go ahead.');
			row = await settle(caseId);
		}
		const calls = await callsOf(caseId);
		if ((!OPEN.has(row.status) || row.alert) && (!o.until || o.until(calls, row))) {
			if (!r.cases.includes(caseId)) r.cases.push(caseId);
			return { caseId, calls, row };
		}
	}
	if (caseId && !r.cases.includes(caseId)) r.cases.push(caseId);
	const row = await settle(caseId, 10_000);
	return { caseId, calls: await callsOf(caseId), row };
}
function replyHygiene(r, chat, label, allow = []) {
	const text = chat.all().join('\n');
	r.check(text.trim().length > 0, `${label}: the customer got a reply`);
	r.check(!containsId(text, allow), `${label}: no reply carries a record id`);
	r.check(!ASKS.some((re) => re.test(text)), `${label}: no reply asks for a lookup key`);
}
const hasOps = (r, calls, ops) => {
	for (const op of ops) r.check(named(calls, op).some((c) => c.state === 'ok'), `${op} ran ok`);
};

let ipSequence = Date.now() & 0x1ffff;
const nextIp = () => {
 ipSequence = (ipSequence + 1) & 0x1ffff;
 return `198.${18 + (ipSequence >> 16)}.${(ipSequence >> 8) & 255}.${ipSequence & 255}`;
};
// web chat on the b4 binding (env test): an anonymous visitor or a known customer with a signed assertion (assertionFor)
async function webChat(cust) {
	const body = { widget_key: J.widgetKey };
	if (cust) body.customer_assertion = assertionFor(cust);
	const ip = { 'cf-connecting-ip': nextIp() };
	const s = await edge('POST', '/api/webchat/sessions', body, ip);
	if (s.status !== 201) throw new Error(`web session -> ${short(s)}`);
	const session = s.json.session_token;
	const visitor = randomBytes(10).toString('hex');
	let conversation = null;
	let token = null;
	const heard = [];
	return {
		identified: s.json.identified === true,
		all: () => heard,
		coreId: () => (token ? token.slice(0, token.lastIndexOf('.')).split('~')[0] : null),
		async send(text) {
			const b = { widget_key: J.widgetKey, visitor_id: visitor, messages: [{ role: 'user', text }], message_id: randomBytes(12).toString('hex') };
			if (conversation && token) Object.assign(b, { conversation_id: conversation, token });
			const m = await edge('POST', '/api/webchat/messages', b, { authorization: `Bearer ${session}`, ...ip });
			if (m.status !== 200) throw new Error(`web message -> ${short(m)}`);
			conversation = m.json.conversation_id ?? conversation;
			token = m.json.token ?? token;
			if (m.json.text) heard.push(String(m.json.text));
			return { status: String(m.json.status), text: String(m.json.text ?? '') };
		},
	};
}
const REFUND_ASK = 'I changed my mind about my wool rug, please refund it.';

// ===== authoring (product routes only; idempotent) ============================================================================
// b4's platform rows live in b4's OWN template family, industry_dept/retail/b4_lib: a tag stacks only base, every base_dept
// head and the industry head of the SAME industry (tags.rs stack_for), never another industry_dept of the same rank, so the
// smoke's industry_dept/retail/customer_service and every job built from it never see these rows. Nothing the smoke authored
// is read, patched or tagged here.
const B4_FAMILY = 'industry_dept/retail/b4_lib_v2';
async function authorPlatform() {
	const layer = await ensureLayer({ scope: 'industry_dept', industry: 'retail', department: 'b4_lib_v2', label: B4_FAMILY, areas: [...new Set(Object.values(DATA.spaces).map((x) => x.fields.area))] }, B4_FAMILY);
	if (!layer) throw pgap('layer_missing', `layer ${B4_FAMILY} cannot be created or found (POST /api/layers)`);
	const put = async (kind, key, fields) => {
		try {
			await addRow(layer, kind, key, fields);
		} catch (e) {
			throw pgap(`platform_row_refused:${kind}`, `${key}: ${e.message}`);
		}
	};
	// a variant without its `*` row is not an operation (agent-model catalog.rs resolve_operations) and the family does not
	// carry the smoke's rows: author every `*` row the library needs that DATA lacks, from the contract (opdefs.json)
	const contract = JSON.parse(readFileSync(process.env.PS_OPDEFS, 'utf8'));
	const defs = Array.isArray(contract) ? contract : contract.operations ?? contract.ops ?? [];
	const ops = { ...DATA.ops };
	const wanted = new Set(Object.keys(DATA.ops).map((k) => k.replace(/\{.*\}$/, '')));
	for (const o of templateOps().concat(extraTemplateOps())) wanted.add(`${o.object}.${o.verb}`);
	for (const a of Object.values(DATA.activities)) for (const o of a.fields.operations ?? []) wanted.add(o.op);
	for (const base of wanted) {
		if (ops[base]) continue;
		const d = defs.find((x) => `${x.object}.${x.verb}` === base && x.variant === '*');
		if (!d) continue;
		const w = d.side_effect !== 'R';
		ops[base] = { discriminator: null, attrs: { modality: w ? 'do' : 'observe', is_derive: false, side_effect: d.side_effect, idempotency: w ? 'key' : 'nat', confirm_back: d.side_effect === 'X', verify_read: null, builtin: false, op_kind: 'standard', guard_rules: [], identity_level: String(d.identity_level), input_fields: d.input_fields ?? [], output_fields: d.output_fields ?? [] }, description: `${base} (contract)`, retail: 'R', stages: ['Support'], extra_errors: [] };
	}
	// operations an EARLIER layer of the stack already adds (the base the smoke pinned) must not be added again
	// (compile: "adds operation X, which an earlier layer already adds"): read what the smoke's job already sees, GET only
	const smokeJob = JSON.parse(readFileSync(env.STATE, 'utf8')).job;
	const have = new Set(((await api('ta', 'GET', `/api/jobs/${smokeJob}/operations`)).json?.ops ?? []).map((o) => `${o.object}.${o.verb}${o.variant && o.variant !== '*' ? `{${o.variant}}` : ''}`));
	for (const [k, f] of Object.entries(ops)) if (!have.has(k)) await put('operation', k, f);
    // Publish an additive base revision for policy dates; already published pins stay immutable.
    const orderRead = defs.find(o => o.object === 'order' && o.verb === 'read' && o.variant === '*');
    const platformOps = itemsOf(ok(await api('sa', 'GET', '/api/platform/operations'), 'platform operations'));
    const existingOrder = platformOps.find(o => o.object === 'order' && o.verb === 'read' && o.variant === '*');
    assert(existingOrder, 'the smoke authored order.read');
    if (!orderRead.output_fields.every(field => existingOrder.output_fields.includes(field))) {
        for (const field of ['placed_at', 'delivered_at']) ok(await api('sa', 'PUT', `/api/platform/objects/order/fields/${field}`, { data_type: 'datetime', description: 'External order lifecycle date used by return and warranty policies.' }), `author order.${field}`);
        const attrs = existingOrder;
        ok(await api('sa', 'PUT', '/api/platform/operations/order/read', { identity_level: attrs.identity_level, side_effect: attrs.side_effect, confirm_back: attrs.confirm_back, verify_read: attrs.verify_read, idempotency: attrs.idempotency, input_fields: attrs.input_fields, output_fields: orderRead.output_fields, description: existingOrder.description, extra_errors: existingOrder.extra_errors }), 'publishable order policy outputs');
        ok(await api('sa', 'POST', `/api/platform/templates/${slug(existingOrder.layer_key)}/tags`, { note: 'Order lifecycle dates for policy evaluation' }), 'tag order policy outputs');
    }
    for (const [id, label, keys] of [
        ['RET-01', 'Return window', ['window_days', 'excluded_categories']],
        ['WAR-01', 'Warranty period', ['period_months']],
        ['PAY-04', 'Pending payment', ['drop_off_days', 'customer_text']],
    ]) await put('rule_slot', id, { label, applies_to: [id === 'RET-01' ? 'case.create{return}' : id === 'WAR-01' ? 'case.create{warranty_claim}' : 'payment.refund'], inputs: [], outcome: 'eligible | ineligible | escalate', values: keys.map(key => ({ key, label: key.replaceAll('_', ' '), unit: key.endsWith('_days') ? 'days' : key.endsWith('_months') ? 'months' : 'text', hint: `The ${key.replaceAll('_', ' ')} this business applies.` })) });
	// the job EXPECTS an operation only through an operation_expectation row of a pinned layer (routes_store.rs: summary
	// `required|optional [{variants}]`); the operation rows alone only fill the catalogue
	const exp = {};
	for (const [k, f] of Object.entries(ops)) {
		const m = /^([a-z_]+\.[a-z_]+)(?:\{(.*)\})?$/.exec(k);
		if (!m) continue;
		const e = (exp[m[1]] ??= { req: false, vars: [] });
		e.req ||= f.retail === 'R';
		if (m[2]) e.vars.push(m[2]);
	}
	for (const [k, e] of Object.entries(exp)) await put('operation_expectation', k, { label: k, summary: `${e.req ? 'required' : 'optional'}${e.vars.length ? ` {${e.vars.join(', ')}}` : ''}` });
	for (const [k, a] of Object.entries(DATA.activities)) await put('activity', k, a.fields);
	for (const [k, s] of Object.entries(DATA.spaces)) await put('context_space', k, s.fields);
	let tagged = 0;
	const t = await api('sa', 'POST', `/api/platform/templates/${slug(B4_FAMILY)}/tags`, { note: 'b4 library' });
	if (t.status === 422) throw pgap('platform_tag_refused', `tag ${B4_FAMILY}: ${short(t)} (the authored operations/activities/spaces do not compile)`);
	if (t.status !== 409) { ok(t, `tag ${B4_FAMILY}`, [200, 201]); tagged++; } // 409 = nothing_to_tag
	note(`platform rows (family ${B4_FAMILY}): ${Object.keys(ops).length} operations, ${Object.keys(DATA.activities).length} activities, ${Object.keys(DATA.spaces).length} spaces; ${tagged} tag cut`);
}
async function authorConnector() {
	const body = {
		key: TEMPLATE.key, version: B4_TEMPLATE_VERSION, label: `Demo Retail (REST) v${B4_TEMPLATE_VERSION}`, vendor: 'Nonagentic', system_class: 'commerce', wave: 'v1', transport: 'rest', auth_kind: 'bearer_static', vendor_api_pin: 'demo-backends',
		auth_params: { static_headers: '{"X-Demo-Tenant":"{{tenant.id}}","X-Demo-Env":"{{system.env}}"}' },
		credential_schema: [{ name: 'service_token', label: 'Service token', secret: true }],
		ops: [...new Map([...templateOps(), ...extraTemplateOps()].map(o => [`${o.object}.${o.verb}{${o.variant}}`, o])).values()],
	};
	const c = await api('sa', 'POST', '/api/platform/connector-templates', body);
	if (c.status === 409 && /template_exists/.test(c.text)) return note(`connector template ${TEMPLATE.key}@${B4_TEMPLATE_VERSION} exists already`);
	if (c.status !== 201) throw pgap('connector_template_create', `POST /api/platform/connector-templates ${TEMPLATE.key}@${B4_TEMPLATE_VERSION} -> ${c.status} ${String(c.text).slice(0, 4000)}`);
	const pub = await api('sa', 'POST', `/api/platform/connector-templates/${TEMPLATE.key}/${B4_TEMPLATE_VERSION}/publish`, {});
	if (pub.status !== 200) throw pgap('connector_template_publish', `publish ${TEMPLATE.key}@${B4_TEMPLATE_VERSION} -> ${short(pub)}`);
	note(`connector template ${TEMPLATE.key}@${B4_TEMPLATE_VERSION} authored and published`);
}
async function authorTenant() {
	// 1. external systems from the versioned demo_retail template: prod, and a TEST one that is NOT a stub (faults act on X-Demo-Env: test and the data must change)
	for (const e of ['prod', 'test']) {
		const c = await api('ta', 'POST', '/api/connections/systems', { id: SYS[e], label: `Demo Retail B4 (${e})`, template_key: TEMPLATE.key, template_version: B4_TEMPLATE_VERSION, env: e, stub: false, base_url: B4_DEMO_URL });
		if (c.status === 422 && /invalid_target|invalid_url/.test(c.text)) throw pgap('connector_private_target', `the tenant cannot point an instance at ${B4_DEMO_URL}: ${short(c)}`);
		if (c.status !== 409) ok(c, `POST /api/connections/systems ${SYS[e]}`, [201]);
		ok(await api('ta', 'PUT', `/api/connections/systems/${SYS[e]}/credential`, { service_token: B4_DEMO_TOKEN }), `PUT credential ${SYS[e]}`);
	}
	// 2. a NEW job from the newest tagged template (a job pins its tags at creation)
	const jobs = itemsOf(ok(await api('ta', 'GET', '/api/jobs'), 'GET /api/jobs'));
	if (!jobs.some((j) => j.key === JOB_KEY)) {
		const cat = ok(await api('ta', 'GET', '/api/job-templates'), 'GET /api/job-templates')?.templates ?? [];
		const ver = (t) => Number(/@v(\d+)$/.exec(t.template)?.[1] ?? 0);
		const cands = cat.filter((t) => t.template.startsWith(`${B4_FAMILY}@`));
		const tpl = (cands.length ? cands : [])
			.sort((a, b) => ver(b) - ver(a))[0];
		if (!tpl) throw pgap('no_job_template', 'GET /api/job-templates offers no template');
		const made = await api('ta', 'POST', '/api/jobs', { template: tpl.template, key: JOB_KEY, label: 'Order and payment support' });
		if (made.status !== 201) throw pgap('job_create', `POST /api/jobs ${tpl.template} -> ${short(made)}`);
		note(`job ${JOB_KEY} created from ${tpl.template}`);
	}
	JOB = JOB_KEY;
	J.job = JOB;
	// the release target selects what the job may use (areas -> context spaces, channel, business model), as the smoke's hierarchy step does
	{
		const t = ok(await api('ta', 'GET', jp('/target')), 'GET target');
		const areas = [...new Set(Object.values(DATA.spaces).map((x) => x.fields.area))];
		const sel = { ...(t?.selection ?? {}), industry: 'retail', departments: [B4_FAMILY], areas, profile: 'retail_ecommerce', dimensions: { ...(t?.selection?.dimensions ?? {}), channel: ['web-chat'] } };
		ok(await api('ta', 'PUT', jp('/target'), { selection: sel, expected_updated_at: t?.selection_updated_at ?? null }), 'PUT target (selection)');
	}
	// Not a product gap: a release cut adopts newer tags of the job's pinned layers (POST /api/jobs/{job}/releases
	// `adopt_pins`). The B4 library is its own department family, which the smoke job never pinned, so it gets its own job.
	note(`the B4 library is department ${B4_FAMILY}; the smoke job does not pin it, so ${JOB} is a separate job`);
	// 3. the customer web chat binding (stays env=test: draft design, faults apply) + identity secret
	const bs = itemsOf(ok(await api('ta', 'GET', jp('/channel-bindings')), 'GET channel-bindings'));
	let b = bs.find((x) => x.kind === 'web_chat' && x.label === BINDING_LABEL);
	if (!b) b = ok(await api('ta', 'POST', jp('/channel-bindings'), { kind: 'web_chat', side: 'customer', label: BINDING_LABEL, settings: { display: { title: 'B4 shop', greeting: 'Hi! How can we help?', colour: '#1f6f5c' }, identity: { mode: 'hs256', issuer: ISSUER, session_hours: 8 } } }), 'POST channel-bindings', [200, 201]);
	J.binding = b.id;
	J.widgetKey = b.external_id;
	const sec = ok(await api('ta', 'POST', jp(`/channel-bindings/${b.id}/identity-secret`), {}), 'POST identity-secret');
	J.idKid = sec?.kid;
	J.idSecret = sec?.secret;
	assert(J.widgetKey && J.idKid && J.idSecret, `binding without widget key / identity secret: ${JSON.stringify(b).slice(0, 200)}`);
	if (b.env && b.env !== 'test') throw pgap('binding_not_test', `the b4 binding is env ${b.env}: demo faults would not apply`);
	// 4. staff team
	assert(who.ag?.sub, 'no staff agent in the journey state');
	const staff = await api('ta', 'PUT', jp('/staff'), { members: [who.ag.sub], routing: { mode: 'off' } });
	if (staff.status === 422 || staff.status === 409) throw pgap(`staff_${staff.json?.code ?? staff.status}`, `the agent cannot join the b4 job staff team: ${short(staff)}`);
	ok(staff, 'PUT staff');
	// 5. routes: every authored operation, test -> the non-stub test system, prod -> the prod system, built-ins in-process
	const refused = [];
	let routed = 0;
	// every operation the job expects (the smoke's order.list/party.read/... come from the stack's base, not from DATA.ops)
	const expected = (ok(await api('ta', 'GET', jp('/operations')), 'GET job operations')?.ops ?? []).map((o) => `${o.object}.${o.verb}${o.variant && o.variant !== '*' ? `{${o.variant}}` : ''}`);
	for (const op of [...new Set([...Object.keys(DATA.ops), ...expected])]) {
		for (const e of ['test', 'prod']) {
			const target = /^case\.create\{(handover|complaint)\}$/.test(op) ? { kind: 'agent_cases' } : BUILTIN.test(op) ? { kind: 'builtin' } : { kind: 'instance', instance_id: SYS[e] };
			const r = await api('ta', 'PUT', jp(`/operations/${encodeURIComponent(op)}/routes/${e}`), { target });
			if (isOk(r)) routed++;
			else if (!(r.status === 422 && /not_implemented/.test(r.text))) refused.push(`${op}@${e}: ${r.status} ${String(r.text).slice(0, 1500)}`);
		}
	}
	if (refused.length) throw pgap('route_refused', `${refused.length} routes refused: ${[refused[0], ...refused.slice(1).map((x) => x.slice(0, 40))].join(' | ')}`);
	note(`${routed} routes set on ${JOB} (ops the template does not implement stay unrouted: party.list/payment.create/case.comment are optional grants)`);
    for (const [id, values] of Object.entries({
        'RET-01': { window_days: '30', excluded_categories: 'perishable, personalised' },
        'WAR-01': { period_months: '24' },
        'PAY-04': { drop_off_days: '7', customer_text: 'A pending reservation is released by your bank; it is not a captured payment.' },
    })) ok(await api('ta', 'PUT', jp(`/rules/${id}`), { values }), `configure ${id}`);
	await resetDemo();
}
async function authorProcedures() {
	const out = [];
	for (const p of DATA.procedures) {
		const c = await api('ta', 'POST', jp('/procedures'), { name: p.name, version: p.version, context_space: p.context_space, description: p.description, steps: p.steps, success_criteria: [], tool_requirements: p.tool_requirements });
		if (c.status === 409 && /version_exists/.test(c.text)) out.push(`${p.name}@${p.version} exists`);
		else {
			if (!isOk(c)) throw pgap(`procedure_${c.json?.code ?? c.json?.error?.code ?? c.status}`, `POST procedures ${p.name}@${p.version} on ${p.context_space} -> ${short(c)}`);
			if ((c.json?.outside_grant ?? []).length) throw pgap('operation_outside_grant', `${p.name}@${p.version} uses ${JSON.stringify(c.json.outside_grant)} outside the activity grant ${JSON.stringify(c.json.activity_grant)}`);
		}
		for (const to of ['CANARY', 'PUBLISHED']) {
			const s = await api('ta', 'POST', jp(`/procedures/${encodeURIComponent(p.name)}/versions/${p.version}/status`), { to });
			if (s.status >= 400 && !(s.status === 409 && /invalid_transition/.test(s.text))) throw pgap(`procedure_${to.toLowerCase()}`, `${p.name}@${p.version} -> ${to}: ${short(s)}`);
		}
	}
	note(`${DATA.procedures.length} procedures authored and published on ${JOB}`);
}

// ===== the rows ===============================================================================================================
const SP = {
	track: 'retail:logistics:track_parcel', status: 'retail:order-support:order_status', problem: 'retail:logistics:delivery_problem', redirect: 'retail:logistics:redirect',
	cancel: 'retail:order-support:cancel_order', returns: 'retail:order-support:returns', refunds: 'retail:payments:refunds', refund: 'retail:payments:refund_request',
	dup: 'retail:payments:payment_errors', pending: 'retail:payments:pending_charge', warranty: 'retail:order-support:warranty', complaint: 'customer_service:complaints:complaint_intake', rights: 'customer_service:privacy:data_rights',
};
const ROWS = [];
const row = (d) => ROWS.push(d);
const webCaseOf = async (core) => {
	if (!core) return { caseId: null, row: null, calls: [] };
	const c = await api('ta', 'GET', jp(`/conversations/${encodeURIComponent(core)}`));
	const caseId = isOk(c) ? (c.json?.conversation?.case_id ?? null) : null;
	return caseId ? { caseId, row: await settle(caseId), calls: await callsOf(caseId) } : { caseId: null, row: null, calls: [] };
};

row({
	id: 'A7', arm: 'staff', title: 'X op with a verify fault ends unknown_outcome, never retried', spaces: [SP.refund],
	plan: ['PUT demo-backends /admin/faults {payment.refund, applied_timeout, 1}', 'staff test chat as the logged-in customer: refund ask, then "yes" at the confirm-back',
		'after the external refund record appears: PUT /admin/faults {payment.list, error, 1} (the verify read fails)',
		'assert call unknown_outcome, case alert + handover item unknown_outcome, ONE payment.refund row, ONE refund in demo-backends, no host/url/code in the reply',
		'POST operation-calls/{id}/check {verdict: landed}: alert cleared, still one call, one refund'],
	async run(r, x) {
        await refundableOrder(r);
		await setFault('payment.refund', 'applied_timeout', 1);
		const before = await refundCount('RET-01');
        r.note(`External refund baseline: ${before}`);
        const verification = failRefundVerification(before);
        const chat = await staffChat(P.c1001);
        let caseId;
        let armed;
        try {
            await chat.send(REFUND_ASK);
            caseId = await waitCase(chat);
            if (!caseId) throw new Error('no case after the refund request');
            r.cases.push(caseId);
            x.a7Case = caseId;
            if ((await caseRow(caseId)).status === 'confirmation_required') await chat.send('Yes, please go ahead.');
            armed = await verification.done;
        } finally {
            verification.stop();
        }
        r.check(armed, 'the external refund was observed before arming the verify-read fault');
		const row2 = await settle(caseId);
		const calls = named(await callsOf(caseId), 'payment.refund');
		r.check(calls.length === 1, `exactly one payment.refund call row (never retried), got ${calls.length}`);
		r.check(calls[0]?.state === 'unknown_outcome', `the payment.refund call ends unknown_outcome, got ${calls[0]?.state}`);
		r.check(row2.alert === 'unknown_outcome', `the case alert is unknown_outcome, got ${row2.alert}`);
		const q = await handoverItems(caseId);
		r.check(q.some((i) => i.reason === 'unknown_outcome'), `a handover item has reason unknown_outcome (reasons: ${q.map((i) => i.reason).join(',') || 'none'})`);
		r.check((await refundCount('RET-01')) - before === 1, 'demo-backends shows exactly one refund (the write was applied)');
		const text = chat.all().join('\n');
		r.check(!LEAK.some((re) => re.test(text)) && !containsId(text), 'the customer-facing text carries no host, URL, code or id');
		if (calls[0]) {
			const chk = await api('ta', 'POST', jp(`/operation-calls/${encodeURIComponent(calls[0].call_id)}/check`), { verdict: 'landed', note: 'b4 acceptance: the refund is in the demo-backends records' });
			r.check(isOk(chk), `check landed accepted (${chk.status})`);
			r.check(!(await caseRow(caseId)).alert, 'the alert cleared after the check');
			r.check(named(await callsOf(caseId), 'payment.refund').length === 1, 'still one payment.refund call after the check (not retried)');
			r.check((await refundCount('RET-01')) - before === 1, 'still one refund after the check');
		}
	},
});

row({
	id: 'A13', arm: 'staff', title: 'a staff member decides a manual handover: reason "Staff decision"', spaces: [SP.refund],
	plan: ['staff test chat as the customer: refund request with applied_timeout (active, no handover item)', 'the staff agent (ag) POST cases/{id}/handover {reason: staff_decision}: handover item reason staff_decision',
		"repeat on A7's case when A7 ran in this run and RECORD what the queue shows (not a PASS criterion)"],
	async run(r, x) {
        await refundableOrder(r);
        await setFault('payment.refund', 'applied_timeout', 1);
        const chat = await staffChat(P.c1001);
        await chat.sendNoWait(REFUND_ASK);
        const caseId = await waitCase(chat);
        assert(caseId, 'manual handover needs an active case');
        r.cases.push(caseId);
        const cr = await caseRow(caseId);
		r.check(OPEN.has(cr.status), `the refund case stays active during the external response stall, got ${cr.status}`);
		r.check((await handoverItems(caseId)).length === 0, 'no open handover item before the staff decision');
		const h = await api('ag', 'POST', jp(`/cases/${encodeURIComponent(caseId)}/handover`), { reason: 'staff_decision' });
		if (h.status === 403) throw pgap('staff_cannot_hand_over', `the staff agent (support_agent) may not POST cases/{id}/handover: ${short(h)}`);
		r.check(isOk(h), `POST cases/{id}/handover accepted (${h.status})`);
		const q = await handoverItems(caseId);
		r.check(q.some((i) => i.reason === 'staff_decision'), `the handover item reason is staff_decision (got ${q.map((i) => i.reason).join(',') || 'none'})`);
		const src = join(REPO, 'web/src/lib/journey/J21Handover.ts');
		if (existsSync(src)) r.check(/staff_decision:\s*\{\s*label:\s*['"]Staff decision['"]/.test(readFileSync(src, 'utf8')), 'web J21Handover.ts labels staff_decision "Staff decision"');
		if (x.a7Case) {
			const h2 = await api('ag', 'POST', jp(`/cases/${encodeURIComponent(x.a7Case)}/handover`), { reason: 'staff_decision' });
			const q2 = await handoverItems(x.a7Case);
			r.note(`RECORDED (unspecified combination) staff handover on the unknown_outcome case: POST ${h2.status}; items now: ${q2.map((i) => `${i.reason}/${i.state}`).join(', ') || 'none'}`);
		} else r.note("RECORDED: A7's case is not available in this run; the combination was not observed");
	},
});

row({
	id: 'A16', arm: 'staff', title: 'second payment.refund in one case is denied case_cap_reached', spaces: [SP.refund],
	plan: ['GET procedures/refund_request; author refund_request@top+1 = the authored steps with gates/preconditions cleared + a second payment.refund step (next phase)', 'approve (optional review mark), status CANARY then PUBLISHED',
		'staff test chat as the customer: refund ask + yes', 'assert one payment.refund ok, the second denied case_cap_reached, case handed over tool_failed, one refund in demo-backends', 'finally PUBLISHED -> ARCHIVED so TEST is back on the authored row'],
	async run(r) {
        await refundableOrder(r);
		const base = DATA.procedures.find((p) => p.name === 'refund_request');
		const list = await api('ta', 'GET', jp('/procedures/refund_request'));
		if (!isOk(list)) throw pgap('refund_request_missing', `GET procedures/refund_request -> ${short(list)}: the authored procedure is not in the job`);
		const top = Math.max(0, ...itemsOf(list.json).map((v) => Number(v.version?.version ?? v.version ?? 0)));
		const steps = base.steps.map((s) => ({ ...s, preconditions: [], gate: false }));
		const refund = steps.find((s) => s.operation === 'payment.refund');
		steps.push({ ...refund, step_number: steps.length + 1, phase: Math.max(...steps.map((s) => s.phase)) + 1, name: 'refund2', max_per_case: 1, gate: false, preconditions: [] });
		const version = top + 1;
		const created = await api('ta', 'POST', jp('/procedures'), { name: 'refund_request', version, context_space: base.context_space, description: 'b4 acceptance A16: refund_request with a second payment.refund (cap test); TEMPORARY', steps, success_criteria: [], tool_requirements: base.tool_requirements });
		if (created.status === 403) throw pgap('designer_role_missing', 'the tenant admin lacks job_designer (cannot create the draft procedure version)');
		if (!isOk(created)) throw new Error(`createProcedureVersion -> ${short(created)}`);
		let status = 'DRAFT';
		const path = (s) => jp(`/procedures/refund_request/versions/${version}/${s}`);
		try {
			for (const to of ['CANARY', 'PUBLISHED']) {
				const st = await api('ta', 'POST', path('status'), { to });
				r.check(isOk(st), `status -> ${to} accepted (${st.status})`);
				if (isOk(st)) status = to;
                if (to === 'CANARY' && isOk(st)) {
                    const ap = await api('ta', 'POST', path('approve'));
                    r.check(isOk(ap), `approve CANARY accepted (${short(ap)})`);
                }
			}
			const before = await refundCount('RET-01');
			const chat = await staffChat(P.c1001);
			const { calls, row: cr } = await drive(r, chat, REFUND_ASK, { until: (c) => named(c, 'payment.refund').length >= 2 });
			const refunds = named(calls, 'payment.refund');
			r.check(refunds.filter((c) => c.state === 'ok').length === 1, `exactly one payment.refund ok, got ${refunds.map((c) => c.state).join(',')}`);
			const denied = refunds.filter((c) => c.state === 'denied');
			r.check(denied.length === 1 && String(denied[0].outcome).includes('case_cap_reached'), `the second payment.refund is denied case_cap_reached, got ${denied.map((c) => c.outcome).join(',') || 'none'}`);
			r.check(cr.status === 'handover' && cr.escalation_reason === 'tool_failed', `the case is handed over with reason tool_failed, got ${cr.status}/${cr.escalation_reason}`);
			r.check((await refundCount('RET-01')) - before === 1, 'demo-backends shows exactly one refund');
		} finally {
			const back = await api('ta', 'POST', path('status'), { to: status === 'PUBLISHED' ? 'ARCHIVED' : 'DISCARDED' });
			assert(isOk(back), `archive temporary refund procedure: ${short(back)}`);
            const restored = await api('ta', 'POST', jp(`/procedures/refund_request/versions/${base.version}/status`), { to: 'PUBLISHED' });
            assert(isOk(restored), `restore original refund procedure: ${short(restored)}`);
		}
	},
});

row({
	id: 'W0', arm: 'web', title: 'web: an anonymous visitor reaches no data; the signed-in customer is answered without being asked for a key', spaces: [SP.track],
	plan: ['anonymous session (no assertion): "Where is my order?" -> sign-in invitation, zero operation calls, nothing asked for', 'signed assertion for the customer (customerChat): "Where is my parcel?" -> the in-transit mug set named, a delivery read, no id, no ask'],
	async run(r) {
		const anon = await webChat(null);
		const a = await anon.send('Where is my order?');
		r.check(/sign(ed)? in|log ?in/i.test(a.text), 'the anonymous visitor is told to sign in');
		r.check((await webCaseOf(anon.coreId() ?? '')).calls.length === 0, 'the anonymous visitor caused zero operation calls');
		r.check(!ASKS.some((re) => re.test(anon.all().join('\n'))), 'the anonymous visitor is not asked for an order number, email or postcode');
        const known = await webChat(P.c1001);
        await known.send('Where is my parcel?');
        const wc = await webCaseOf(known.coreId());
        if (wc.caseId) r.cases.push(wc.caseId);
        r.check(/mug/i.test(known.all().join(' ')), 'the signed-in customer is answered with the in-transit order named by product');
        r.check(named(wc.calls, 'delivery.list').length + named(wc.calls, 'delivery.read').length > 0, 'a delivery read ran for the signed-in customer');
        replyHygiene(r, known, 'W0 signed-in');
	},
});

row({
	id: 'W1', arm: 'web', title: 'web: a forged identity assertion is refused three times and reaches no data', spaces: [],
	plan: ['three tampered customer_assertion values at POST /api/webchat/sessions -> each refused 401', 'an anonymous follow-up still reaches no operation call; demo-backends orders unchanged'],
	async run(r) {
		const good = assertionFor(P.c1001);
		const [h, p, sig] = good.split('.');
		const orders = JSON.stringify(await demoRows('orders'));
		for (let i = 1; i <= 3; i++) {
			const s = await edge('POST', '/api/webchat/sessions', { widget_key: J.widgetKey, customer_assertion: `${h}.${p}.${sig.slice(0, -4)}${i}abc` }, { 'cf-connecting-ip': nextIp() });
			r.check(s.status === 401, `forged assertion ${i} is refused 401 (got ${s.status})`);
		}
		const anon = await webChat(null);
		await anon.send('Where is my parcel?');
		r.check((await webCaseOf(anon.coreId() ?? '')).calls.length === 0, 'no operation call after the refused assertions');
		r.check(JSON.stringify(await demoRows('orders')) === orders, 'no demo activity: orders unchanged');
		r.note('the retired plan W1 (wrong email x3 -> identity_failed + locked session) is the verification path the 2026-10-26 ruling removed; not driven');
	},
});

row({
	id: 'W2', arm: 'web', title: 'web: a signed-in refund request resolves with one refund', spaces: [SP.refund],
	plan: ['signed assertion for the customer: refund ask, then "yes"', 'payment.list, order.read, payment.refund ok; resolved; one refund in demo-backends'],
	async run(r) {
        await refundableOrder(r);
		const before = await refundCount('RET-01');
		const w = await webChat(P.c1001);
		await w.send(REFUND_ASK);
		await w.send('Yes, please go ahead.');
		const wc = await webCaseOf(w.coreId() ?? '');
		if (wc.caseId) r.cases.push(wc.caseId);
		hasOps(r, wc.calls, ['payment.list', 'order.read', 'payment.refund']);
		r.check(wc.row?.status === 'resolved', `the case resolved, got ${wc.row?.status}`);
		r.check((await refundCount('RET-01')) - before === 1, 'exactly one refund in demo-backends');
		replyHygiene(r, w, 'web refund');
	},
});

row({
	id: 'W3', arm: 'web', title: "web: a signed-in customer cannot cancel someone else's order", spaces: [SP.cancel],
	plan: ['signed assertion for the first customer: "Please cancel my oak side table order" (that order is the second customer\'s)', 'no order.cancel ok, order unchanged, no id in the reply; outcome recorded'],
	async run(r) {
		const status0 = await orderStatus('RET-03');
		const w = await webChat(P.c1001);
		await w.send('Please cancel my oak side table order.');
		let wc = await webCaseOf(w.coreId() ?? '');
		if (wc.row?.status === 'confirmation_required') {
			await w.send('Yes, please go ahead.');
			wc = await webCaseOf(w.coreId() ?? '');
		}
		if (wc.caseId) r.cases.push(wc.caseId);
		r.check(!named(wc.calls, 'order.cancel').some((c) => c.state === 'ok'), 'no order.cancel succeeded');
		r.check((await orderStatus('RET-03')) === status0, "the other customer's order is unchanged");
		replyHygiene(r, w, 'web cross-customer');
		r.note(`RECORDED: case ${wc.row?.status}/${wc.row?.escalation_reason ?? '-'}; calls ${wc.calls.map((c) => `${c.op}:${c.state}`).join(', ') || 'none'}`);
	},
});

row({
	id: 'W-A7', arm: 'web', title: 'web: a refund with a verify fault ends unknown_outcome, never retried', spaces: [SP.refund],
	plan: ['PUT /admin/faults {payment.refund, applied_timeout, 1}', 'signed assertion: refund ask, then "yes" if confirmation is required; monitor the external write concurrently', 'after the external refund record appears: PUT /admin/faults {payment.list, error, 1}',
		'assert unknown_outcome call, alert, handover item unknown_outcome, one call row, one refund, no host/url/code in the reply (the b4 binding is env test, so the fault applies)'],
	async run(r) {
        await refundableOrder(r);
		await setFault('payment.refund', 'applied_timeout', 1);
		const before = await refundCount('RET-01');
        r.note(`External refund baseline: ${before}`);
        const verification = failRefundVerification(before);
        const w = await webChat(P.c1001);
        let wc;
        let caseId;
        let armed;
        try {
            await w.send(REFUND_ASK);
            wc = await webCaseOf(w.coreId() ?? '');
            if (!wc.caseId) throw new Error('the web chat opened no case');
            r.cases.push(wc.caseId);
            caseId = wc.caseId;
            if (wc.row?.status === 'confirmation_required') await w.send('Yes, please go ahead.');
            armed = await verification.done;
        } finally {
            verification.stop();
        }
		wc = await webCaseOf(w.coreId() ?? '');
		const calls = named(wc.calls, 'payment.refund');
		if (calls.length && calls[0].state === 'ok' && (await refundCount('RET-01')) - before === 1) throw pgap('demo_fault_not_applied', 'the refund went through ok: the fault did not fire on this binding (the route is not the env=test non-stub system?)');
		r.check(armed, 'the external refund was observed before arming the verify-read fault');
		r.check(calls.length === 1, `exactly one payment.refund call row (never retried), got ${calls.length}`);
		r.check(calls[0]?.state === 'unknown_outcome', `the call ends unknown_outcome, got ${calls[0]?.state}`);
		r.check(wc.row?.alert === 'unknown_outcome', `the case alert is unknown_outcome, got ${wc.row?.alert}`);
		const q = await handoverItems(caseId);
		r.check(q.some((i) => i.reason === 'unknown_outcome'), `a handover item has reason unknown_outcome (got ${q.map((i) => i.reason).join(',') || 'none'})`);
		r.check((await refundCount('RET-01')) - before === 1, 'demo-backends shows exactly one refund (the write was applied)');
		const text = w.all().join('\n');
		r.check(!LEAK.some((re) => re.test(text)) && !containsId(text), 'the customer-facing text carries no host, URL, code or id');
	},
});

row({
	id: 'L1', arm: 'staff', title: 'order status (and csat); and an order without a shipment', spaces: [SP.status],
	plan: ['the first customer: "Where is my wool rug order?" -> order.read, delivery.read, resolved; csat 5 submitted for the case', 'the third customer: "Where is my linen bedding order?" -> order.read; outcome RECORDED'],
	async run(r) {
		const a = await staffChat(P.c1001);
		const x1 = await drive(r, a, 'Where is my wool rug order?');
		hasOps(r, x1.calls, ['order.read', 'delivery.read']);
		r.check(x1.row.status === 'resolved', `resolved, got ${x1.row.status}`);
		replyHygiene(r, a, 'L1 rug');
		const cs = await api('ta', 'POST', jp(`/conversations/${encodeURIComponent(a.id)}/csat`), { case_id: x1.caseId, rating: 5 });
		r.check(cs.status === 404 && /csat_not_requested/.test(cs.text), `staff test channel does not request a rating (${short(cs)})`);
		const w = await webChat(P.c1001);
        await w.send('Where is my wool rug order?');
        const resolved = await webCaseOf(w.coreId());
        r.check(resolved.row?.status === 'resolved', 'web customer order resolves');
        const thanks = await w.send('Thank you!');
        r.check(/Reply 1[–-]5/.test(thanks.text), 'the customer acknowledgement receives the CSAT prompt');
        const rated = await w.send('5');
        r.check(/thank you for your rating/i.test(rated.text), 'the customer rating is recorded');
		const b = await staffChat(P.c1003);
		const x2 = await drive(r, b, 'Where is my linen bedding order?');
		hasOps(r, x2.calls, ['order.read']);
		replyHygiene(r, b, 'L1 bedding');
		r.note(`RECORDED (order without shipment): ${x2.row.status}/${x2.row.outcome}; calls ${x2.calls.map((c) => `${c.op}:${c.state}`).join(', ')}`);
	},
});

row({
	id: 'L2', arm: 'staff', title: 'track a known customer parcel without a lookup key', spaces: [SP.track],
    plan: ['the first customer: "Can you track my wool rug parcel?"', 'a delivery read ok; resolved; no lookup key asked or exposed'],
    async run(r) {
        const chat = await staffChat(P.c1001);
        const d = await drive(r, chat, 'Can you track my wool rug parcel?');
        r.check(named(d.calls, 'delivery.read').concat(named(d.calls, 'delivery.list')).some((c) => c.state === 'ok'), 'a delivery read ran ok');
        r.check(d.row.status === 'resolved', `resolved, got ${d.row.status}`);
        replyHygiene(r, chat, 'L2');
    },
});

row({
	id: 'L3', arm: 'staff', title: 'late delivery: report logged and handed over', spaces: [SP.problem],
	plan: ['demo-backends scenario late-delivery', 'the first customer: "My mug set parcel is late, it was due yesterday."', 'order.read, delivery.read, delivery.report_problem, case.create{handover} ok; a handover item'],
	async run(r) {
		await scenario('late-delivery');
		const chat = await staffChat(P.c1001);
		const d = await drive(r, chat, 'My mug set parcel is late, it was due yesterday.', { until: (c) => named(c, 'case.create{handover}').length > 0 });
		hasOps(r, d.calls, ['order.read', 'delivery.read', 'delivery.report_problem', 'case.create{handover}']);
		r.check((await handoverItems(d.caseId)).length > 0, 'a handover item exists for the case');
		replyHygiene(r, chat, 'L3');
	},
});

row({
	id: 'L4', arm: 'staff', title: 'redirect a parcel that is on track', spaces: [SP.redirect],
	plan: ['the first customer: "Please send my mug set to Prinsengracht 263 in Amsterdam instead" + yes', 'order.read, delivery.read, delivery.redirect ok; resolved'],
	async run(r) {
		const chat = await staffChat(P.c1001);
		const d = await drive(r, chat, 'Please send my mug set to Prinsengracht 263 in Amsterdam instead.');
		hasOps(r, d.calls, ['order.read', 'delivery.read', 'delivery.redirect']);
		r.check(d.row.status === 'resolved', `resolved, got ${d.row.status}`);
		replyHygiene(r, chat, 'L4');
	},
});

row({
	id: 'L5', arm: 'staff', title: 'cancel: a cancellable order; and one that is not', spaces: [SP.cancel],
	plan: ['the third customer: "Please cancel my linen bedding order" + yes -> order.cancel ok, order cancelled in demo-backends', 'the first customer: "Please cancel my wool rug order" -> order.read only, not_eligible, return offered, no cancel call'],
	async run(r) {
		const a = await staffChat(P.c1003);
		const x1 = await drive(r, a, 'Please cancel my linen bedding order.');
		hasOps(r, x1.calls, ['order.read', 'order.cancel']);
		r.check(x1.row.status === 'resolved', `resolved, got ${x1.row.status}`);
		r.check(/cancel/i.test(await orderStatus('RET-04')), 'the order is cancelled in demo-backends');
		replyHygiene(r, a, 'L5 cancel');
		const b = await staffChat(P.c1001);
		const x2 = await drive(r, b, 'Please cancel my wool rug order.');
		hasOps(r, x2.calls, ['order.read']);
		const refused = named(x2.calls, 'order.cancel');
        r.check(refused.length === 1 && refused[0].state === 'denied' && refused[0].outcome === 'precondition_unmet', 'the delivered order cancellation is denied before an external write');
        r.check(await orderStatus('RET-01') === 'delivered', 'the delivered order is unchanged externally');
		r.check(x2.row.status === 'not_eligible', `not_eligible, got ${x2.row.status}`);
		r.check(/return/i.test(b.all().join(' ')), 'the reply offers the return instead');
		replyHygiene(r, b, 'L5 delivered');
	},
});

row({
	id: 'L6', arm: 'staff', title: 'return: inside the window; and past it', spaces: [SP.returns],
	plan: ['the first customer: "I\'d like to return the rug." -> order.read, rule.evaluate, case.create{return} ok, resolved', 'scenario return-window-expired; the fourth customer: "I want to return the jute doormat." -> rule.evaluate, no return case, not_eligible'],
	async run(r) {
		const a = await staffChat(P.c1001);
		const x1 = await drive(r, a, "I'd like to return the rug.");
		hasOps(r, x1.calls, ['order.read', 'rule.evaluate', 'case.create{return}']);
		r.check(x1.row.status === 'resolved', `resolved, got ${x1.row.status}`);
		replyHygiene(r, a, 'L6 in window');
		await scenario('return-window-expired');
		const b = await staffChat(P.c1004);
		const x2 = await drive(r, b, 'I want to return the jute doormat.');
		hasOps(r, x2.calls, ['order.read', 'rule.evaluate']);
		r.check(named(x2.calls, 'case.create{return}').length === 0, 'no return case past the window');
		r.check(x2.row.status === 'not_eligible', `not_eligible, got ${x2.row.status}`);
		replyHygiene(r, b, 'L6 past window');
	},
});

row({
	id: 'L7', arm: 'staff', title: 'refund status is read-only; a refund once, a second ask refused', spaces: [SP.refunds, SP.refund],
	plan: ['the first customer: "Where is my refund for the rug?" -> payment.list, order.read; no payment.refund; no refund in demo-backends', 'REFUND_ASK + yes in a new chat -> resolved, one refund', 'again in a new chat -> no second refund, not_eligible, demo-backends shows 1 refund'],
	async run(r) {
        await refundableOrder(r);
		const base = await refundCount('RET-01');
		const a = await staffChat(P.c1001);
		const x1 = await drive(r, a, 'Where is my refund for the rug?');
		hasOps(r, x1.calls, ['payment.list', 'order.read']);
		r.check(named(x1.calls, 'payment.refund').length === 0, 'the status question made no payment.refund call');
		r.check((await refundCount('RET-01')) === base, 'the status question changed no refund');
		replyHygiene(r, a, 'L7 status');
		const b = await staffChat(P.c1001);
		const x2 = await drive(r, b, REFUND_ASK);
		r.check(named(x2.calls, 'payment.refund').some((c) => c.state === 'ok'), 'the refund ran ok');
		r.check(x2.row.status === 'resolved', `resolved, got ${x2.row.status}`);
		r.check((await refundCount('RET-01')) - base === 1, 'one refund in demo-backends');
		replyHygiene(r, b, 'L7 refund');
		const c = await staffChat(P.c1001);
		const x3 = await drive(r, c, REFUND_ASK);
		r.check(named(x3.calls, 'payment.refund').filter((k) => k.state === 'ok').length === 0, 'the second ask made no second refund');
		r.check(x3.row.status === 'not_eligible', `second ask is not_eligible, got ${x3.row.status}`);
		r.check((await refundCount('RET-01')) - base === 1, 'demo-backends still shows 1 refund');
		r.note(`second ask: ${x3.row.status}/${x3.row.outcome}`);
	},
});

row({
	id: 'L8', arm: 'staff', title: 'payment problem: duplicate charge; pending charge -> dispute', spaces: [SP.dup, SP.pending],
	plan: ['scenario duplicate-charge; the first customer: "I think I was charged twice for my rug."', 'scenario pending-charge; the third customer: "The payment for my linen bedding still shows as pending."', 'order.read, payment.list, rule.evaluate, case.create{dispute} ok'],
	async run(r) {
		await scenario('duplicate-charge');
		const a = await staffChat(P.c1001);
		const x1 = await drive(r, a, 'I think I was charged twice for my rug.');
		hasOps(r, x1.calls, ['order.read', 'payment.list', 'rule.evaluate', 'case.create{dispute}']);
		replyHygiene(r, a, 'L8 duplicate');
		await scenario('pending-charge');
		const b = await staffChat(P.c1003);
		const x2 = await drive(r, b, 'The payment for my linen bedding still shows as pending.');
		hasOps(r, x2.calls, ['order.read', 'payment.list', 'rule.evaluate', 'case.create{dispute}']);
		replyHygiene(r, b, 'L8 pending');
	},
});

row({
	id: 'L9', arm: 'staff', title: 'warranty claim', spaces: [SP.warranty],
	plan: ['scenario warranty-defect; the second customer: "My oak side table is broken, it should still be under warranty." + a follow-up about the defect', 'order.read, rule.evaluate, case.create{warranty_claim} ok; resolved'],
	async run(r) {
		await scenario('warranty-defect');
		const chat = await staffChat(P.c1002);
		const d = await drive(r, chat, 'My oak side table is broken, it should still be under warranty.', { follow: ['The legs wobble and one has cracked.'] });
		hasOps(r, d.calls, ['order.read', 'rule.evaluate', 'case.create{warranty_claim}']);
		r.check(d.row.status === 'resolved', `resolved, got ${d.row.status}`);
		replyHygiene(r, chat, 'L9');
	},
});

row({
	id: 'L10', arm: 'staff', title: 'complaint: complaint case + handover item', spaces: [SP.complaint],
	plan: ['the first customer: "I want to complain about how my last order was handled." + natural follow-ups', 'case.create{complaint} ok; its handover item exists before any further step'],
	async run(r) {
		const chat = await staffChat(P.c1001);
		const d = await drive(r, chat, 'I want to complain about how my last order was handled.', {
			follow: ['The rug arrived with a frayed edge and nobody got back to me.', 'I would like a replacement or my money back.'],
			until: (c) => named(c, 'case.create{complaint}').length > 0,
		});
		hasOps(r, d.calls, ['case.create{complaint}']);
		r.check((await handoverItems(d.caseId)).length > 0, 'a handover item exists for the case');
		replyHygiene(r, chat, 'L10');
	},
});

row({
	id: 'L11', arm: 'staff', title: 'data rights request -> privacy handover item', spaces: [SP.rights],
	plan: ['the first customer: "Please delete all my personal data." (+ yes)', 'case.create{handover} ok; a handover item exists'],
	async run(r) {
		const chat = await staffChat(P.c1001);
		const d = await drive(r, chat, 'Please delete all my personal data.', { follow: ['Yes, please pass that on to your privacy team.'], until: (c) => named(c, 'case.create{handover}').length > 0 });
		hasOps(r, d.calls, ['case.create{handover}']);
		const q = await handoverItems(d.caseId);
		r.check(q.length > 0, 'a handover item exists for the case');
		r.note(`RECORDED: handover item(s) ${q.map((i) => `${i.reason}/${i.queue ?? '-'}/${i.subject ?? '-'}`).join('; ') || 'none'}`);
		replyHygiene(r, chat, 'L11');
	},
});

const ORDER = ['A7', 'A13', 'A16', 'W0', 'W1', 'W2', 'W3', 'W-A7', 'L1', 'L2', 'L3', 'L4', 'L5', 'L6', 'L7', 'L8', 'L9', 'L10', 'L11'];
ROWS.sort((a, b) => ORDER.indexOf(a.id) - ORDER.indexOf(b.id));

// ===== main ===================================================================================================================
const selected = ONLY.length ? ROWS.filter((d) => ONLY.includes(d.id)) : ROWS;
if (ONLY.length && selected.length !== ONLY.length) {
	console.error(`unknown row in --only (rows: ${ORDER.join(' ')})`);
	process.exit(2);
}
const AUTHORING_PLAN = [
	`sa   POST /api/layers (ensure the 5 smoke layers), POST /api/layers/{id}/rows: ${Object.keys(DATA.ops).length} operation, ${Object.keys(DATA.activities).length} activity, ${Object.keys(DATA.spaces).length} context_space rows; POST /api/platform/templates/{layer}/tags`,
	`sa   POST /api/platform/connector-templates demo_retail@${B4_TEMPLATE_VERSION} (smoke ops + ${extraTemplateOps().length} more), POST .../publish`,
	`ta   POST /api/connections/systems ${SYS.prod} (prod) and ${SYS.test} (env test, stub false), PUT .../credential`,
	`ta   GET /api/job-templates, POST /api/jobs {template: newest tag, key: ${JOB_KEY}}`,
	`ta   POST /api/jobs/${JOB_KEY}/channel-bindings (web_chat, stays env test), POST .../identity-secret, PUT .../staff {agent}`,
	`ta   PUT /api/jobs/${JOB_KEY}/operations/{op}/routes/{test,prod} for ${Object.keys(DATA.ops).length} ops (built-ins in-process)`,
	`ta   POST /api/jobs/${JOB_KEY}/procedures x${DATA.procedures.length} (${DATA.procedures.map((p) => `${p.name}@${p.version}`).join(', ')}), POST .../status CANARY, PUBLISHED`,
	'demo-backends  POST /admin/reset {seed:42} before every row; PUT /admin/faults (A7, W-A7); POST /admin/scenarios/{name}; GET /admin/records/{table}',
];

if (LIST) {
	for (const d of ROWS) console.log(`${d.id.padEnd(5)} ${d.arm.padEnd(5)} ${d.title}`);
	process.exit(0);
}
if (DRY) {
	console.log('DRY RUN: nothing is sent. Authoring (product routes, idempotent):');
	AUTHORING_PLAN.forEach((p, i) => console.log(`  A${i + 1}. ${p}`));
	console.log(`\nRows run after the smoke on its stack (state ${process.env.PS_STATE ?? '$PS_STATE'}, report ${REPORT}); each starts with demo-backends POST /admin/reset:`);
	for (const d of selected) {
		console.log(`\n${d.id} [${d.arm}] ${d.title}\n  spaces: ${d.spaces.join(', ') || '-'}`);
		d.plan.forEach((p, i) => console.log(`  ${i + 1}. ${p}`));
	}
	process.exit(0);
}

const loopback = (u) => /^https?:\/\/(localhost|127\.0\.0\.1|\[::1\])(:\d+)?$/.test(u);
if (!loopback(env.API) || !loopback(env.EDGE) || /:8000(\/|$)/.test(env.API + env.EDGE)) {
	console.error(`refusing: the API (${env.API}) and edge (${env.EDGE}) must be loopback and never port 8000`);
	process.exit(2);
}

const report = { at: new Date().toISOString(), api: env.API, edge: env.EDGE, tenant: null, job: JOB_KEY, authoring: [], rows: [], gaps };
let ready = true;
let notReady = '';
const authoring = async (id, title, fn) => {
	if (SKIP_AUTHORING && id !== 'B4-A0') return;
	const before = results.length;
	await step(id, title, async () => {
		if (!ready) throw new Error(`skipped: ${notReady}`);
		try {
			await fn();
		} catch (e) {
			ready = false;
			notReady = `${id} failed: ${String(e.message).slice(0, 200)}`;
			throw e;
		}
	});
	report.authoring.push({ id, title, pass: results[before]?.[1] === true });
};

await authoring('B4-A0', 'journey state of the smoke (tenant, agent) is loaded', async () => {
	const path = env.STATE;
	if (!path || !existsSync(path)) throw pgap('no_journey_state', `PS_STATE ${path || '(unset)'} does not exist: run platform-stack.sh smoke first`);
	const st = JSON.parse(readFileSync(path, 'utf8'));
	if (!st.tenant || !st.admin?.sub) throw pgap('journey_state_incomplete', `${path} holds no tenant / admin: the smoke did not get past step 1`);
	J.tenant = st.tenant;
	report.tenant = st.tenant;
	who.ta = { persona: 'ta', sub: st.admin.sub, email: st.admin.email };
	if (st.agent?.sub) who.ag = { persona: 'ag', sub: st.agent.sub, email: st.agent.email };
	await mint('ta');
	if (SKIP_AUTHORING) {
		JOB = JOB_KEY;
		J.job = JOB;
		const bs = itemsOf(ok(await api('ta', 'GET', jp('/channel-bindings')), 'GET channel-bindings'));
		const b = bs.find((x) => x.kind === 'web_chat' && x.label === BINDING_LABEL);
		if (!b) throw pgap('skip_authoring_without_authoring', 'the b4 binding does not exist: run once without --skip-authoring');
		const sec = ok(await api('ta', 'POST', jp(`/channel-bindings/${b.id}/identity-secret`), {}), 'POST identity-secret');
		Object.assign(J, { binding: b.id, widgetKey: b.external_id, idKid: sec.kid, idSecret: sec.secret });
	}
});
await authoring('B4-A1', 'platform content: operations, activities, spaces authored and tagged by sa', authorPlatform);
await authoring('B4-A2', `connector template demo_retail@${B4_TEMPLATE_VERSION} authored and published by sa`, authorConnector);
await authoring('B4-A3', 'tenant: systems, b4 job, web chat binding, staff, routes, demo data', authorTenant);
await authoring('B4-A4', 'the 13 library procedures authored and published through the procedure route', authorProcedures);
await authoring('B4-A5', 'the known customers of the external system', async () => {
	await resetDemo();
	const cs = await demoCustomers();
	for (const n of ['1001', '1002', '1003', '1004']) {
		P[`c${n}`] = cs.find((c) => c.customer_id === `cust-${n}`);
		if (!P[`c${n}`]) throw pgap('demo_persona_missing', `demo-backends holds no cust-${n} (customers: ${cs.map((c) => c.customer_id).join(', ')})`);
	}
});

const shared = { a7Case: null };
for (const def of selected) {
	const rec = { id: def.id, title: def.title, arm: def.arm, status: 'PASS', reason: '', checks: [], notes: [], cases: [] };
	const r = {
		cases: rec.cases,
		check: (c, what) => rec.checks.push({ ok: !!c, what }),
		note: (t) => {
			rec.notes.push(t);
			note(t);
		},
	};
	await step(def.id, def.title, async () => {
		try {
			if (!ready) throw new Error(`authoring incomplete (${notReady})`);
			await needSpaces(def.spaces);
			await resetDemo();
			await def.run(r, shared);
			const bad = rec.checks.filter((c) => !c.ok);
			if (!rec.checks.length) throw new Error('row asserted nothing');
			if (bad.length) throw new Error(bad.map((c) => c.what).join('; '));
		} catch (e) {
			rec.status = 'FAIL';
			rec.reason = String(e.message ?? e);
			throw e;
		} finally {
			await clearFaults();
		}
	});
	report.rows.push(rec);
}

const passed = report.rows.filter((x) => x.status === 'PASS').length;
console.log(`\nB4 ROWS: ${passed}/${report.rows.length} passed${gaps.length ? `; PRODUCT GAP codes: ${gaps.join(', ')}` : ''}`);
try {
	mkdirSync(dirname(REPORT), { recursive: true });
	writeFileSync(REPORT, JSON.stringify({ ...report, summary: { total: report.rows.length, passed, failed: report.rows.length - passed } }, null, 2));
	console.log(`report: ${REPORT}`);
} catch (e) {
	console.log(`report not written: ${e.message}`);
}
process.exitCode = finish();
