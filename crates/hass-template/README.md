# Native templates

`TemplateEngine` evaluates bounded MiniJinja expressions and complete templates
against current Runtime state. It provides native state helpers, numeric defaults,
date/time filters and the configured Home Assistant timezone. It has no filesystem
loader or shell access. Source and output are limited to 64 KiB, recursion to 64
levels, and evaluation to 25,000 fuel units.

`render` returns text. `render_value` preserves single-expression JSON types and
also renders multiple expression nodes or control blocks. Complete-template
results parse decimal numbers, `True`/`False`/`None`, and the supported JSON-compatible
Python container subset; other output remains text. Quoted strings retain their
quotes, and top-level scientific notation remains text, following pinned result
parsing. Unsupported Python literal forms, including tuple/set and containers
with Python-only values, and complete Python/Jinja formatting parity remain open.

The actual process webhook fixture exercises adjacent expressions in a service
action value. Package fixtures cover typed values, composite/block rendering,
state updates, numeric failures, configured timezone and resource bounds. Source
anchors are `helpers/template/__init__.py` and its tests at Home Assistant Core
`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`.
