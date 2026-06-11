---
name: home-assistant
description: Control and query a Home Assistant smart home via its REST API. Triggers: home assistant, smart home, lights, light on, light off, turn on, turn off, switch, thermostat, climate, temperature setpoint, dim, brightness, scene, cover, blinds, garage, sensor state, 智能家居, 灯, 开灯, 关灯, 温控.
version: 1.0.0
author: octos
always: false
---

# Home Assistant

Read and control a Home Assistant smart home through the official REST API
(`/api/...`). Use it to check entity states (lights, switches, sensors,
climate), list what devices exist, and call services to actually control
devices.

## Configuration (environment variables)

Both must be set or every tool fails with a clear message:

- `HA_URL` (required): Base URL of your Home Assistant instance including
  scheme, host, and port — for example `https://ha.example.com:8123` or
  `http://192.168.1.10:8123`. Do **not** include a trailing `/api`; a trailing
  `/` is trimmed automatically.
- `HA_TOKEN` (required): A Home Assistant **long-lived access token**, created
  in Home Assistant under your user Profile → "Long-Lived Access Tokens".

Every request sends `Authorization: Bearer <HA_TOKEN>`.

## Tools

### ha_get_states

Read the current state of entities.

```json
{"entity_id": "light.kitchen"}
```

**Parameters:**
- `entity_id` (optional): An exact entity_id (e.g. `light.kitchen`) to fetch a
  single entity. If it isn't an exact match it is treated as a case-insensitive
  substring filter over entity_id and friendly name. Omit to list all entities
  (output is capped to avoid flooding).

### ha_call_service

Call a service to control a device.

```json
{"domain": "light", "service": "turn_on", "entity_id": "light.kitchen", "data": {"brightness_pct": 60}}
```

**Parameters:**
- `domain` (required): Service domain, e.g. `light`, `switch`, `climate`.
- `service` (required): Service name, e.g. `turn_on`, `turn_off`, `toggle`,
  `set_temperature`.
- `entity_id` (optional): Target entity id, or array of ids.
- `data` (optional): Extra service parameters merged into the request body
  (e.g. `{"brightness_pct": 60, "temperature": 21}`).

Returns a summary of the entities whose state changed, or
"No state changes reported." if the response was empty.

### ha_list_entities

List entities grouped by domain with a per-domain count.

```json
{"domain": "light"}
```

**Parameters:**
- `domain` (optional): Restrict the listing to a single domain (e.g. `light`).
  Omit to summarize all domains.
