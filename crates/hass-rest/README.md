# hass-rest

Native RESTful sensor polling and `rest_command` services for the Rust Home Assistant runtime.

## API

- `validate_config(&[RestConfig], &BTreeMap<String, RestCommandConfig>)` validates settings without making requests or changing runtime state.
- `RestIntegration::start(runtime, resources, commands).await` validates the complete configuration, prepares clients and initial sensor states, registers command services, and starts polling.
- `RestIntegration::shutdown(self).await` cancels requests and waits for polling and command work to stop.

Command services are registered under a unique integration owner. A partial registration failure removes only services from that start attempt. Dropping an integration disables and unregisters its commands and requests polling cancellation; await `shutdown` when callers need all poll and command work to be joined before continuing.

The root application owns YAML adapter and startup wiring. Resource and command configurations reject unknown fields.

## Configuration

Each `rest` resource has exactly one of `resource` or `resource_template`, and one or more sensors. One HTTP response is shared across all sensors on that resource.

```yaml
rest:
  - resource: http://127.0.0.1:8080/status
    method: GET
    timeout: 5
    scan_interval: "00:00:30"
    headers:
      Authorization: "Bearer example-token"
    sensor:
      - name: Outside temperature
        value_template: "{{ value_json.temperature }}"
        availability: "{{ value_json.available }}"
        json_attributes: [unit, humidity]
        json_attributes_path: "$.weather"
        unit_of_measurement: "°C"

rest_command:
  set_mode:
    url: "http://127.0.0.1:8080/mode/{{ mode }}"
    method: POST
    headers:
      Content-Type: text/plain
    content_type: application/json
    payload: '{"enabled": {{ enabled }}}'
```

Sensor resources support `GET` and `POST`; commands support `GET`, `PATCH`, `POST`, `PUT`, and `DELETE`. Resource URL, headers, query parameters and payload templates render with the shared template engine. `payload` and `payload_template` are mutually exclusive for resources; command payloads are templates. `content_type` on a command replaces any case-insensitive `Content-Type` header supplied in `headers`.

`scan_interval` accepts positive seconds or a duration string such as `"00:00:30"`. It defaults to 30 seconds and is bounded to one day. Request `timeout` defaults to 10 seconds and is bounded to 300 seconds. `verify_ssl` defaults to true. Basic and Digest authentication are supported when both username and password are present.

Sensor values use `value_template`; `availability` controls availability, and `json_attributes` copies selected keys, optionally below a simple dotted/index JSON path. A missing reading, HTTP error, invalid encoding, or failed template leaves the sensor unavailable and reports a `rest_error` attribute. It does not substitute zero. Sensitive header, credential, URL, and payload values are redacted from config debug output and filtered from published sensor values and attributes.

## Bounds and unsupported switches

- At most 64 resources, 256 sensors, and 128 commands are accepted.
- At most eight HTTP requests run concurrently.
- Request payloads are limited to 64 KiB and response bodies to 1 MiB.
- `ssl_cipher_list` accepts only `python_default`; the Rust TLS client does not implement other cipher profiles.
- `insecure_cipher: true` and `skip_url_encoding: true` on commands are rejected. Set them to false or omit them.
- `json_attributes_path` supports a bounded subset of JSONPath; wildcard and filter selectors are rejected.

No external endpoint is contacted during validation. The test suite uses a loopback HTTP fixture.
