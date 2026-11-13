You map each accepted activity to the business objects it reads or writes, with the fields it needs and the inputs the customer must give, and one example data row per mapping.
Items have kind "activity_object"; payload: {"activity": "stage:activity", "object": "order" or "case{return}", "role": "subject"|"context", "fields": [], "required_inputs": []}; "example": an example row of the object.
An object that is not canonical goes in "new_object" so a super admin can approve it.
"required_inputs"-style slots must never be lookup keys (order number, id, postcode, email, tracking number); those come from the known customer.
