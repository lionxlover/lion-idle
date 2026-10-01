# lion-config schema assumed by lion-idle

`lion-idle`'s own build prompt didn't include `lion-config`'s contract, so
this is the minimal schema lion-idle expects until that component's real
one is available. Not loaded by lion-idle itself -- just documentation
for whoever implements `lion-config` (or an admin driving it directly).

Schema: `org.lionos.idle`

| Key           | Type | Meaning                              | Default (if unset/unreachable) |
|---------------|------|---------------------------------------|---------------------------------|
| `dim-ms`      | u64  | Backlight dim threshold, 0 = disabled | disabled                        |
| `dpms-ms`     | u64  | Display off threshold, 0 = disabled   | 120000 (2 min)                  |
| `lock-ms`     | u64  | Lock-screen threshold, 0 = disabled   | 300000 (5 min)                  |
| `suspend-ms`  | u64  | Suspend threshold, 0 = disabled       | 1800000 (30 min)                |

lion-idle re-reads all four whenever `lion-config` emits `Changed` for
this schema -- see `src/config.rs`.

Notes for implementers (behavior lion-idle relies on):

- A `Get` that *fails* (service down, unknown key) falls back to the
  default above -- thresholds degrade to safe values, never to "off".
  A value that *succeeds* is trusted as-is, including an explicit `0`
  (that key's action disabled).
- A returned value of the wrong type (e.g. a string where a u64
  belongs) reads as "disabled" for that key, with a warning in the
  daemon's log -- the one case where a successful reply does not win.
- Values are milliseconds; thresholds beyond ~49 days clamp to u32::MAX
  when creating the Wayland notification.
- The suspend threshold locks the session before suspending regardless
  of `lock-ms` (see README: "Suspend implies lock").
