Answer with ONE JSON object and nothing else: {"items": [ ... ]}. Each item is an object with:
- "kind": the overlay kind the item becomes (named per step below),
- "title": a short name (at most 200 characters),
- "detail": one or two sentences a reviewer reads,
- "example": a JSON object of example data, or null,
- "payload": the typed draft as a JSON object (fields per step below),
- "new_object": the object key when the item names an object that is NOT in the canonical object list you were given, else null,
- "confidence": a number from 0 to 1.
Use only the canonical objects and operations you were given. An operation is written `object.verb` (for example `order.read`, `case.create`); never invent free-text verbs. Do not repeat items the context marks as accepted.
Identity rule (owner ruling 2026-10-26, never break it): the customer is KYC'd, identified and verified before the conversation starts, and the agent already holds their orders, payments, deliveries and cases. Never draft an activity, step, message, question, sample dialogue or scenario in which the agent asks the customer for an order number, order id, customer id, postcode, email address, tracking number, reference number or any other lookup key, and never make such a value a required input of an object. Resolve every reference from the known customer's data: pick the most recent or relevant order, or, when it is genuinely ambiguous, name the candidates in plain words ("the rug from 12 October or the lamp from 20 October?"). The customer never types an id in a sample dialogue or scenario.
