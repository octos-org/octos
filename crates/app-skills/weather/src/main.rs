//! Weather skill: get current weather via Open-Meteo (free, no API key).
//!
//! Protocol: `./main <tool_name>` with JSON on stdin, JSON on stdout.

use std::io::Read;
use std::thread::sleep;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct GetWeatherInput {
    city: String,
}

#[derive(Deserialize)]
struct GeoResult {
    results: Option<Vec<GeoLocation>>,
}

#[derive(Deserialize)]
struct GeoLocation {
    latitude: f64,
    longitude: f64,
    name: String,
    country: Option<String>,
    admin1: Option<String>,
    timezone: Option<String>,
    #[serde(default)]
    population: Option<u64>,
}

#[derive(Deserialize)]
struct GetForecastInput {
    city: String,
    #[serde(default = "default_days")]
    days: u8,
}

fn default_days() -> u8 {
    7
}

#[derive(Deserialize)]
struct WeatherResponse {
    current: Option<CurrentWeather>,
}

#[derive(Deserialize)]
struct ForecastResponse {
    daily: Option<DailyForecast>,
}

#[derive(Deserialize)]
struct DailyForecast {
    time: Vec<String>,
    temperature_2m_max: Vec<Option<f64>>,
    temperature_2m_min: Vec<Option<f64>>,
    weather_code: Vec<Option<u32>>,
    precipitation_sum: Vec<Option<f64>>,
    wind_speed_10m_max: Vec<Option<f64>>,
}

#[derive(Deserialize)]
struct CurrentWeather {
    temperature_2m: Option<f64>,
    relative_humidity_2m: Option<f64>,
    apparent_temperature: Option<f64>,
    weather_code: Option<u32>,
    wind_speed_10m: Option<f64>,
    wind_direction_10m: Option<f64>,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let tool_name = args.get(1).map(|s| s.as_str()).unwrap_or("unknown");

    let mut buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
        fail(&format!("Failed to read stdin: {e}"));
    }

    match tool_name {
        "get_weather" => handle_get_weather(&buf),
        "get_forecast" => handle_get_forecast(&buf),
        _ => fail(&format!(
            "Unknown tool '{tool_name}'. Expected: get_weather, get_forecast"
        )),
    }
}

fn fail(msg: &str) -> ! {
    println!("{}", json!({"output": msg, "success": false}));
    std::process::exit(1);
}

fn http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(4))
        .connect_timeout(Duration::from_secs(2))
        .build()
        .expect("failed to build HTTP client")
}

fn get_json_with_retries<T>(
    client: &reqwest::blocking::Client,
    url: &str,
    label: &str,
) -> Result<T, String>
where
    T: DeserializeOwned,
{
    const ATTEMPTS: usize = 3;
    let mut last_error = None;

    for attempt in 1..=ATTEMPTS {
        match client.get(url).send().and_then(|r| r.error_for_status()) {
            Ok(response) => match response.json::<T>() {
                Ok(value) => return Ok(value),
                Err(error) => {
                    last_error = Some(format!("Failed to parse {label} response: {error}"));
                }
            },
            Err(error) => {
                last_error = Some(format!("{label} request failed: {error}"));
            }
        }

        if attempt < ATTEMPTS {
            sleep(Duration::from_millis(200 * attempt as u64));
        }
    }

    Err(last_error.unwrap_or_else(|| format!("{label} request failed")))
}

/// Geocode a city name to coordinates.
/// Tries without language hint first, then falls back to zh, ja, ko, ru, ar, hi.
fn geocode(client: &reqwest::blocking::Client, city: &str) -> GeoLocation {
    let encoded = urlencoded(city);
    let has_non_ascii = city.bytes().any(|b| b > 127);

    // Always try without language hint first (finds well-known cities by any name),
    // then for non-ASCII input also try language-specific hints as fallback.
    let langs: &[&str] = if has_non_ascii {
        &["", "zh", "ja", "ko", "ru", "ar", "hi"]
    } else {
        &[""]
    };

    for lang in langs {
        let url = if lang.is_empty() {
            format!("https://geocoding-api.open-meteo.com/v1/search?name={encoded}&count=5")
        } else {
            format!("https://geocoding-api.open-meteo.com/v1/search?name={encoded}&count=5&language={lang}")
        };

        if let Ok(geo) = get_json_with_retries::<GeoResult>(client, &url, "Geocoding") {
            if let Some(mut results) = geo.results {
                if !results.is_empty() {
                    // Pick the most populated result (avoids returning tiny hamlets)
                    results.sort_by_key(|result| std::cmp::Reverse(result.population.unwrap_or(0)));
                    return results.into_iter().next().unwrap();
                }
            }
        }
    }

    fail(&format!("City '{}' not found. Please retry with the English/romanized city name (e.g. 'Saratoga' instead of '萨拉托加', 'Tokyo' instead of '東京').", city));
}

fn weather_description(code: u32) -> &'static str {
    match code {
        0 => "Clear sky",
        1 => "Mainly clear",
        2 => "Partly cloudy",
        3 => "Overcast",
        45 | 48 => "Foggy",
        51 => "Light drizzle",
        53 => "Moderate drizzle",
        55 => "Dense drizzle",
        56 | 57 => "Freezing drizzle",
        61 => "Slight rain",
        63 => "Moderate rain",
        65 => "Heavy rain",
        66 | 67 => "Freezing rain",
        71 => "Slight snow",
        73 => "Moderate snow",
        75 => "Heavy snow",
        77 => "Snow grains",
        80 => "Slight rain showers",
        81 => "Moderate rain showers",
        82 => "Violent rain showers",
        85 => "Slight snow showers",
        86 => "Heavy snow showers",
        95 => "Thunderstorm",
        96 | 99 => "Thunderstorm with hail",
        _ => "Unknown",
    }
}

fn wind_direction(degrees: f64) -> &'static str {
    match ((degrees + 22.5) / 45.0) as u32 % 8 {
        0 => "N",
        1 => "NE",
        2 => "E",
        3 => "SE",
        4 => "S",
        5 => "SW",
        6 => "W",
        7 => "NW",
        _ => "N",
    }
}

fn handle_get_weather(input_json: &str) {
    let input: GetWeatherInput = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(e) => fail(&format!("Invalid input: {e}")),
    };

    if input.city.trim().is_empty() {
        fail("'city' must not be empty");
    }

    let client = http_client();
    let location = geocode(&client, &input.city);

    // Fetch current weather
    let weather_url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&current=temperature_2m,relative_humidity_2m,apparent_temperature,weather_code,wind_speed_10m,wind_direction_10m&timezone={}",
        location.latitude,
        location.longitude,
        location.timezone.as_deref().unwrap_or("auto")
    );

    let weather: WeatherResponse = match get_json_with_retries(&client, &weather_url, "Weather") {
        Ok(v) => v,
        Err(error) => fail(&error),
    };

    let current = match weather.current {
        Some(c) => c,
        None => fail("No current weather data available"),
    };

    let city_display = format!(
        "{}{}{}",
        location.name,
        location
            .admin1
            .as_ref()
            .map(|a| format!(", {a}"))
            .unwrap_or_default(),
        location
            .country
            .as_ref()
            .map(|c| format!(", {c}"))
            .unwrap_or_default(),
    );

    let temp = current
        .temperature_2m
        .map(|t| format!("{t:.1}°C"))
        .unwrap_or_default();
    let feels = current
        .apparent_temperature
        .map(|t| format!("{t:.1}°C"))
        .unwrap_or_default();
    let humidity = current
        .relative_humidity_2m
        .map(|h| format!("{h:.0}%"))
        .unwrap_or_default();
    let conditions = current
        .weather_code
        .map(weather_description)
        .unwrap_or("Unknown");
    let wind = current
        .wind_speed_10m
        .map(|s| {
            let dir = current.wind_direction_10m.map(wind_direction).unwrap_or("");
            format!("{s:.1} km/h {dir}")
        })
        .unwrap_or_default();

    let output = format!(
        "{city_display}\n{conditions}\nTemperature: {temp} (feels like {feels})\nHumidity: {humidity}\nWind: {wind}"
    );

    println!("{}", json!({"output": output, "success": true}));
}

fn handle_get_forecast(input_json: &str) {
    let input: GetForecastInput = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(e) => fail(&format!("Invalid input: {e}")),
    };

    if input.city.trim().is_empty() {
        fail("'city' must not be empty");
    }

    let days = input.days.clamp(1, 16);
    let client = http_client();
    let location = geocode(&client, &input.city);

    // Fetch daily forecast
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&daily=temperature_2m_max,temperature_2m_min,weather_code,precipitation_sum,wind_speed_10m_max&forecast_days={}&timezone={}",
        location.latitude,
        location.longitude,
        days,
        location.timezone.as_deref().unwrap_or("auto")
    );

    let resp: ForecastResponse = match get_json_with_retries(&client, &url, "Forecast") {
        Ok(v) => v,
        Err(error) => fail(&error),
    };

    let daily = match resp.daily {
        Some(d) => d,
        None => fail("No forecast data available"),
    };

    let city_display = format!(
        "{}{}{}",
        location.name,
        location
            .admin1
            .as_ref()
            .map(|a| format!(", {a}"))
            .unwrap_or_default(),
        location
            .country
            .as_ref()
            .map(|c| format!(", {c}"))
            .unwrap_or_default(),
    );

    let mut lines = vec![format!("{city_display} — {days}-day forecast")];

    for i in 0..daily.time.len() {
        let date = &daily.time[i];
        let hi = daily
            .temperature_2m_max
            .get(i)
            .and_then(|v| *v)
            .map(|t| format!("{t:.0}°C"))
            .unwrap_or_default();
        let lo = daily
            .temperature_2m_min
            .get(i)
            .and_then(|v| *v)
            .map(|t| format!("{t:.0}°C"))
            .unwrap_or_default();
        let cond = daily
            .weather_code
            .get(i)
            .and_then(|v| *v)
            .map(weather_description)
            .unwrap_or("—");
        let rain = daily
            .precipitation_sum
            .get(i)
            .and_then(|v| *v)
            .map(|p| {
                if p > 0.0 {
                    format!(" | {p:.1}mm")
                } else {
                    String::new()
                }
            })
            .unwrap_or_default();
        let wind = daily
            .wind_speed_10m_max
            .get(i)
            .and_then(|v| *v)
            .map(|w| format!(" | {w:.0}km/h"))
            .unwrap_or_default();

        lines.push(format!("{date}: {lo}–{hi}  {cond}{rain}{wind}"));
    }

    println!("{}", json!({"output": lines.join("\n"), "success": true}));
}

fn urlencoded(s: &str) -> String {
    let mut result = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(b as char);
            }
            _ => {
                result.push_str(&format!("%{:02X}", b));
            }
        }
    }
    result
}
