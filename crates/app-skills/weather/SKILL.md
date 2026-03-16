---
name: weather
description: Get current weather for any city worldwide. Triggers: weather, forecast, temperature, 天气, 气温, how cold, how hot, is it raining, wind.
version: 1.0.0
author: octos
always: false
---

# Weather

Get current weather conditions for any city worldwide using the free Open-Meteo API (no API key required).

## Tools

### get_weather

Returns current temperature, humidity, wind speed, and weather conditions for a given city.

```json
{"city": "Stockholm"}
```

**Parameters:**
- `city` (required): City name, optionally with country. Examples: `Stockholm`, `Tokyo`, `New York, US`, `上海`, `Paris, France`

### get_forecast

Returns multi-day weather forecast with daily high/low temperatures, conditions, precipitation, and wind.

```json
{"city": "Paris", "days": 7}
```

**Parameters:**
- `city` (required): City name, optionally with country
- `days` (optional): Number of forecast days, 1-16. Default: 7
