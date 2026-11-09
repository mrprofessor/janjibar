# Fixtures

`freelens-promql.json` holds every query Freelens builds, for every provider,
with fixed options (see `OPTS` in the script). `tests/promql.rs` checks that
the crate builds the same strings.

It was made from Freelens commit `3f2b73426bab36e68c31bb2c403e815187148ad7`
(MIT, see `NOTICE`):

```sh
git clone --depth 1 https://github.com/freelensapp/freelens /tmp/freelens
python3 -I extract_freelens_promql.py \
  /tmp/freelens/packages/technical-features/prometheus/src > freelens-promql.json
```

The script reads the TypeScript as text and fills in the templates. It never
runs Freelens code.
