---
name: smart-home
description: List and control smart-home devices (lights, thermostats, switches, covers, speakers) via the profile's configured bridge. Triggers: smart home, turn on/off, lights, thermostat, dim, brightness, temperature, unlock, devices, 智能家居, 开灯, 关灯, 空调, 窗帘, 灯光, 设备.
version: 1.0.0
author: octos
always: false
---

# Smart Home

List and control smart-home devices through the bridge configured on this
profile (e.g. Home Assistant or a compatible gateway). Camera video is not
available through this skill — it is a UI-only feature in the octos-web
dashboard.

Requires a bridge to be configured for the profile first (Settings →
Smart Home). If it isn't configured yet, tools will report that clearly
instead of failing silently.

## Tools

### smart_home_list_devices

Returns every device the bridge knows about, with its current state
(on/off, brightness, temperature, and other fields the device reports).
Use this first to find a device's exact `device_id` before controlling it.

```json
{"room": "Living Room"}
```

**Parameters:**
- `room` (optional): Filter to devices in a given room, case-insensitive. Omit to list everything.

### smart_home_control_device

Sends a command to one device by ID.

```json
{"device_id": "light.living_room_lamp", "params": {"on": true, "brightness": 60}}
```

**Parameters:**
- `device_id` (required): Exact ID from `smart_home_list_devices`.
- `params` (required): Object of fields to set. Common examples:
  - `{"on": true}` / `{"on": false}` — power switches, lights, plugs
  - `{"brightness": 0-100}` — dimmable lights
  - `{"temperature": <number>}` — thermostats
  - `{"volume": 0-100}` — speakers, TVs
  - `{"position": 0-100}` — covers, blinds, curtains
  - `{"color": "#rrggbb"}` — color-capable lights
  - `{"mode": "..."}` — multi-mode appliances (fans, HVAC, etc.)

  Supported fields vary by device kind — use the fields already present on
  that device in `smart_home_list_devices`'s output as a guide.
