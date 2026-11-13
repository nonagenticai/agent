# Runbook — Telegram test bot (`@raisemytickets_bot`)

B7 T15b/T15c, owner rulings R6/R7 (2026-10-21), D75. One bot, one job, one allowed user: the owner's Telegram account.
The signup bot (`@nonagenticaibot`, `TELEGRAM_BOT_TOKEN`) is a different bot and is not touched by any step here.

How it works: the bot is a B12 job channel binding of kind `telegram` (customer side). Its token and webhook secret are
the binding's credential (sealed in the `agent` DB, never returned by the API). Telegram calls
`https://chat.nonagentic.ai/api/channels/telegram/webhook/<binding id>` with the header
`X-Telegram-Bot-Api-Secret-Token`; the edge checks it, drops senders outside `settings.allowed_user_ids`, and hands the
message to agent-api (stored, deduped on `update_id`); the answer goes out with `sendMessage`.

Rules: the token and the secret never go into git in plaintext, a PR, an issue, chat, or the web UI's label/settings.
Do not paste them on a shell command line either — use `read -rs` as below.

## Prerequisites

- `agent-edge` runs with `EDGE_CORE_URL` + `EDGE_INTERNAL_KEY` (the Telegram connector is registered only then):
  `curl -s https://chat.nonagentic.ai/ready` lists `{"kind":"telegram",…}` under `channels`.
- `agent-api` has `EDGE_DELIVER_URL` + `EDGE_INTERNAL_KEY` (needed for the test message).
- The DB constraint `connector_bindings_kind_check` admits `telegram` (a migration widening it must have run; without it
  creating the binding fails).
- A job exists for the owner's test tenant; you hold a token for a user with `integrations_manager` in that tenant.

## 1. Secrets (once)

1. The owner opens @BotFather → `/mybots` → `@raisemytickets_bot` → API Token. If the token was ever shared, use
   `/revoke` first and take the new one. The owner hands it to the operator out of band.
2. Generate the webhook secret: `openssl rand -hex 32`.
3. Store both in `agent-secrets` (SOPS, gitops-production `clusters/strato/services/agent/secrets.sops.yaml`) as
   `TELEGRAM_TEST_BOT_TOKEN` and `TELEGRAM_TEST_BOT_WEBHOOK_SECRET`. This is the record of truth for the operator; no pod
   reads these keys — the live copy is the binding's sealed credential.

## 2. Owner's user id

Before any webhook is set, the owner sends `hi` to the bot, then:

```sh
read -rs TG_TOKEN   # paste the bot token, Enter
curl -s "https://api.telegram.org/bot${TG_TOKEN}/getUpdates" | jq '.result[].message.from | {id, username}'
```

The owner's numeric `id` is the allow-list. (If a webhook is already set, `getUpdates` answers 409: run step 5's
`deleteWebhook` first.)

## 3. Create the binding

```sh
read -rs TG_SECRET  # paste the webhook secret, Enter
API=https://agent.nonagentic.ai; JOB='<job key>'; TENANT='<tenant uuid>'; BEARER='<access token>'
BOT_ID="${TG_TOKEN%%:*}"
jq -n --arg bot "$BOT_ID" --arg t "$TG_TOKEN" --arg s "$TG_SECRET" --argjson owner <owner id> \
  '{kind:"telegram", side:"customer", external_id:$bot, label:"Telegram test bot",
    settings:{allowed_user_ids:[$owner]}, credential:{bot_token:$t, webhook_secret:$s}}' |
curl -s -X POST "$API/api/jobs/$JOB/channel-bindings" -H "Authorization: Bearer $BEARER" \
  -H "X-Active-Tenant-Id: $TENANT" -H 'Content-Type: application/json' --data @- | jq '{id, verified, has_credential}'
```

Expect `verified: true` (the token proves the bot), `has_credential: true`. Keep the `id` as `BINDING`. The edge picks
the binding up within 60 s.

## 4. Register the webhook

```sh
curl -s "https://api.telegram.org/bot${TG_TOKEN}/setWebhook" \
  --data-urlencode "url=https://chat.nonagentic.ai/api/channels/telegram/webhook/${BINDING}" \
  --data-urlencode "secret_token=${TG_SECRET}" \
  --data-urlencode 'allowed_updates=["message","callback_query"]' \
  -d drop_pending_updates=true
curl -s "https://api.telegram.org/bot${TG_TOKEN}/getWebhookInfo" | jq '.result | {url, pending_update_count, last_error_message}'
```

This replaces the old webhook (its host is dead, R7).

## 5. Verify

- Test message: `curl -s -X POST "$API/api/jobs/$JOB/channel-bindings/$BINDING/test-message" -H "Authorization: Bearer
  $BEARER" -H "X-Active-Tenant-Id: $TENANT"` → `{"delivered": true, "message_id": "…"}` and the owner's chat shows
  `Test from Agent: Telegram test bot is connected to job <job>.` (409 `test_chat_not_configured` = no allowed user and no
  `settings.test_chat_id`; 502 `channel_send_failed` = the edge has not loaded the binding yet, or Telegram refused.)
  Telegram only lets a bot message a user who has started it: before the first `/start` the edge logs
  `channel_deliver_failed … telegram_403_403` ("bot can't initiate conversation with a user"). Have the allowed user
  send `/start` first. 2026-10-21 the 403 persisted after a working `/start` exchange — open item in the B7 plan.
- The owner writes to the bot → the job's answer (typing indicator first); the conversation shows in the job's console.
- Anyone else writes → no answer; `kubectl -n agent logs deploy/agent-edge | grep channel_update_not_allowed`.
- `getWebhookInfo` → `pending_update_count` 0, no `last_error_message`.

## Group / forum topic (owner's supergroup)

The bot also serves groups and supergroups, including a forum topic (owner decision 2026-10-22; live: the owner's
forum supergroup `-1004368404922`, the owner's user id `7619636015`). Channel posts are ignored.

1. Add `@raisemytickets_bot` to the group. Either make it an **admin** (admins see every message), or keep it a member
   and turn privacy off first: @BotFather → `/mybots` → the bot → Bot Settings → Group Privacy → **Turn off**, then
   remove and re-add the bot (privacy is applied when the bot joins). With privacy on, a member bot sees only commands
   and replies to itself.
2. Allow-list the people who may talk to it: `allowed_user_ids` holds **user** ids (`from.id`), never the group id.
   Anyone else in the group is acked and dropped (`channel_update_not_allowed`). Messages from bots and service
   messages (topic created/edited, members joined, …) are ignored.
3. Conversations: a message in a forum topic (`is_topic_message` + `message_thread_id`) is the conversation
   `<chat id>:<thread id>` and the answer is sent into that topic (`sendMessage` carries `message_thread_id`). A message
   in the General topic (no thread id) or in a plain group is the conversation `<chat id>`. A private chat stays
   `<chat id>` (= the user id), as before.
4. Test message into a topic: set the group and the topic, then run step 5's test message:

   ```sh
   curl -s -X PUT "$API/api/jobs/$JOB/channel-bindings/$BINDING" -H "Authorization: Bearer $BEARER" \
     -H "X-Active-Tenant-Id: $TENANT" -H 'Content-Type: application/json' \
     --data '{"settings": {"allowed_user_ids": [7619636015], "test_chat_id": -1004368404922, "test_thread_id": <thread id>}}'
   ```

   `test_chat_id` is any non-zero integer (group ids are negative); `test_thread_id` is a positive integer and only
   meaningful with a forum `test_chat_id`. Find the thread id from a topic message: with no webhook set,
   `getUpdates | jq '.result[].message | {chat: .chat.id, thread: .message_thread_id}'`; or from a topic link
   `https://t.me/c/<chat without -100>/<thread id>`. Without `test_thread_id` the test message goes to General.

## Disable / rotate

- Pause: `PUT $API/api/jobs/$JOB/channel-bindings/$BINDING` `{"enabled": false}` — within 60 s the webhook path answers
  404 and nothing reaches the job. Stop Telegram retrying: `curl -s "https://api.telegram.org/bot${TG_TOKEN}/deleteWebhook"`.
- Remove: `DELETE …/channel-bindings/$BINDING`, then `deleteWebhook`.
- Rotate the token: BotFather `/revoke`, update SOPS, `PUT …/channel-bindings/$BINDING` `{"credential": {"bot_token": …,
  "webhook_secret": …}}` (same bot id, or 422), then step 4 again. Rotate the secret the same way (step 4 is required:
  Telegram must send the new secret).
- Change the allow-list: `PUT …` `{"settings": {"allowed_user_ids": [<id>, …]}}` (at most 50; empty = nobody). `PUT`
  replaces the whole `settings` object: send `test_chat_id` / `test_thread_id` again if they are set.

`unset TG_TOKEN TG_SECRET` when done.
