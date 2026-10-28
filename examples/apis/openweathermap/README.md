# OpenWeather

A mock of the OpenWeather APIs for "Gipfelglück", a Munich hiking app: current weather,
the 5 day / 3 hour forecast, One Call 3.0, air pollution and the geocoding API, keyed
with the `appid` query parameter like the real service.

| | |
| --- | --- |
| Contract | [`openweathermap.openapi.yaml`](openweathermap.openapi.yaml), a hand-written subset following the [API docs](https://openweathermap.org/api) |
| Upstream | None: OpenWeather publishes no machine-readable spec (see `upstream.notes` in [`example.json`](example.json)) |
| Hosts | `api.openweathermap.org` |
| Scenarios | `city-not-found` (404), `invalid-api-key` (401), `rate-limited` (429), `outage` (503) |
| Webhooks | None (OpenWeather sends none) |

## Quickstart

```bash
mockingbird examples init openweathermap --dir openweathermap && cd openweathermap   # or: cd examples/apis/openweathermap
mockingbird serve -c mockingbird.yaml --offline
```

In another shell:

```bash
curl -s 'localhost:8080/data/2.5/weather?q=Munich,DE&appid=owm_test_123' -H 'Host: api.openweathermap.org'
```

The [`pyowm`](https://pypi.org/project/pyowm/) client, unmodified, through the proxy
(OpenWeather has no official Python SDK; pyowm is the most used one and calls
`api.openweathermap.org` with `requests`):

```bash
eval "$(mockingbird proxy env)"
pip install pyowm
python - <<'PY'
from pyowm import OWM
owm = OWM("owm_test_123")
w = owm.weather_manager().weather_at_place("Munich,DE").weather
print(w.detailed_status, w.temperature("celsius")["temp"], "°C,", w.humidity, "% humidity")
loc = owm.geocoding_manager().geocode("Munich", country="DE", limit=1)[0]
print(loc.name, loc.country, loc.lat, loc.lon)
PY
```

Drop `--offline` (and configure an LLM, see [the configuration reference](../../../docs/configuration.md#llm))
for answers that follow the persona: the town you asked for with its real coordinates
and city id, late-autumn Alpine temperatures in the units you asked for, German
descriptions with `lang=de`.

## Expected output

`--offline` answers come from the schema faker, so values (and place names) differ run
to run; the shape does not. Temperatures are Kelvin, the API's default `units=standard`:

```json
{
  "coord": {"lat": 48.07, "lon": 11.88},
  "weather": [{"id": 600, "main": "Clear", "description": "few clouds", "icon": "02n"}],
  "base": "stations",
  "main": {"temp": 283.76, "feels_like": 274.15, "temp_min": 270.38, "temp_max": 286.71, "pressure": 1027, "humidity": 54, ...},
  "visibility": 7727,
  "wind": {"speed": 7.39, "deg": 58, "gust": 23.44},
  "clouds": {"all": 67},
  "dt": 1761000093,
  "sys": {"type": 1, "id": 977484, "country": "DE", "sunrise": 1761000028, "sunset": 1761000045},
  "timezone": 7200,
  "id": 2855958,
  "name": "onyx",
  "cod": 200,
  ...
}
```

The Python snippet prints lines like:

```
broken clouds 4.46 °C, 82 % humidity
garnet DE 48.05 10.94
```

## Scenarios

```bash
curl -s 'localhost:8080/data/2.5/weather?q=Atlantis&appid=owm_test_123' -H 'Host: api.openweathermap.org' \
  -H 'X-Mockingbird-Scenario: city-not-found'
# HTTP 404 {"cod":"404","message":"city not found"}
```

pyowm raises `pyowm.commons.exceptions.NotFoundError` for it,
`UnauthorizedError` for `invalid-api-key` (`{"cod":401,"message":"Invalid API key. ..."}`)
and `APIRequestError` for `rate-limited` and `outage`. `outage` answers 503
problem+json, as the contract declares no 5xx body (the real API answers its own
gateway page). Set `default_scenario: city-not-found` on the mock to apply it to every call.
