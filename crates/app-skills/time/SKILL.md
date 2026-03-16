---
name: clock
description: Get current date and time in any timezone. Triggers: time, clock, what time, 几点, 现在时间, 时间, current time, date today, timezone.
version: 1.0.0
author: octos
always: false
---

# Clock

Get the current date and time in any timezone worldwide.

## Tools

### get_time

Returns the current date, time, day of week, and UTC offset for a given timezone.

```json
{"timezone": "Europe/Stockholm"}
```

**Parameters:**
- `timezone` (optional): IANA timezone name. Examples: `UTC`, `Europe/Stockholm`, `Asia/Shanghai`, `US/Eastern`, `Asia/Tokyo`. Default: server local time.

**Common timezones:** UTC, US/Eastern, US/Central, US/Pacific, Europe/London, Europe/Paris, Europe/Stockholm, Europe/Berlin, Asia/Shanghai, Asia/Tokyo, Asia/Seoul, Asia/Singapore, Australia/Sydney
