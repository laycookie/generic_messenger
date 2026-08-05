# Telegram interface

Text-only Telegram user client built on MTProto through `grammers`.

Enable with `record_v2` feature `telegram` (included by feature `full`).

## Login

1. Create Telegram application credentials at <https://my.telegram.org>.
2. Enter API ID, API hash, and account phone number. Submit once.
3. Enter code delivered by Telegram in **Login code**. Submit again.
4. If prompted, enter account two-factor password. Submit again.

Clear **Login code** and submit to request a replacement code.

Successful authorization is stored under `./cache/Telegram/`; later starts
reuse it without another code. Session files grant account access. Keep them
private, do not copy or commit them, and revoke the session from Telegram if
one is exposed.

## Scope

Supported:

- user profile
- dialogs and direct-chat users
- server-side text history with pagination
- plain-text sending
- live new, edited, and deleted text-message events

Not supported:

- media rendering or sending
- formatting, reactions, message mutations, calls
- contact management, dialog creation, or presence
