# Function Sources

Function Source is a database function which can be used to query [vector tiles](https://github.com/mapbox/vector-tile-spec). When started, Martin will look for the functions with a suitable signature. A function that takes `z integer` (or `zoom integer`), `x integer`, `y integer`, and an optional `query json` and returns `bytea`, can be used as a Function Source. Alternatively the function could return a record with a single `bytea` field, or a record with two fields of types `bytea` and `text`, where the `text` field is an etag key (i.e. md5 hash).

| Argument                   | Type    | Description             |
|----------------------------|---------|-------------------------|
| z (or zoom)                | integer | Tile zoom parameter     |
| x                          | integer | Tile x parameter        |
| y                          | integer | Tile y parameter        |
| query (optional, any name) | json    | Query string parameters |

For example, if you have a table `table_source` in WGS84 (`4326` SRID), then you can use this function as a Function Source:

```sql, ignore
CREATE OR REPLACE FUNCTION function_zxy_query(z integer, x integer, y integer) RETURNS bytea AS $$
DECLARE
  mvt bytea;
BEGIN
  SELECT INTO mvt ST_AsMVT(tile, 'function_zxy_query', 4096, 'geom') FROM (
    SELECT
      ST_AsMVTGeom(ST_Transform(ST_CurveToLine(geom), 3857), ST_TileEnvelope(z, x, y), 4096, 64, true) AS geom
    FROM table_source
    WHERE geom && ST_Transform(ST_TileEnvelope(z, x, y), 4326)
  ) as tile WHERE geom IS NOT NULL;

  RETURN mvt;
END
$$ LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE;
```

```sql, ignore
CREATE OR REPLACE FUNCTION function_zxy_query(z integer, x integer, y integer, query_params json) RETURNS bytea AS $$
DECLARE
  mvt bytea;
BEGIN
  SELECT INTO mvt ST_AsMVT(tile, 'function_zxy_query', 4096, 'geom') FROM (
    SELECT
      ST_AsMVTGeom(ST_Transform(ST_CurveToLine(geom), 3857), ST_TileEnvelope(z, x, y), 4096, 64, true) AS geom
    FROM table_source
    WHERE geom && ST_Transform(ST_TileEnvelope(z, x, y), 4326)
  ) as tile WHERE geom IS NOT NULL;

  RETURN mvt;
END
$$ LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE;
```

The `query_params` argument is a JSON representation of the tile request query params. For example, if user requested a tile with [urlencoded](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/encodeURIComponent) params:

```shell
curl \
  --data-urlencode 'arrayParam=[1, 2, 3]' \
  --data-urlencode 'numberParam=42' \
  --data-urlencode 'stringParam=value' \
  --data-urlencode 'booleanParam=true' \
  --data-urlencode 'objectParam={"answer" : 42}' \
  --get localhost:3000/function_zxy_query/0/0/0
```

then `query_params` will be parsed as:

```json
{
  "arrayParam": [1, 2, 3],
  "numberParam": 42,
  "stringParam": "value",
  "booleanParam": true,
  "objectParam": { "answer": 42 }
}
```

You can access this params using [json operators](https://www.postgresql.org/docs/current/functions-json.html):

```sql, ignore
...WHERE answer = (query_params->'objectParam'->>'answer')::int;
```

## Function Source TileJSON

Function Source [TileJSON](https://github.com/mapbox/tilejson-spec) endpoint is available at `/{function_name}`

For example, `points` function will be available at `/points`

```shell
curl localhost:3000/points | jq
```

## Function Source Tiles

Function Source tiles endpoint is available at `/{function_name}/{z}/{x}/{y}`

For example, `points` function will be available at `/points/{z}/{x}/{y}`

```shell
curl localhost:3000/points/0/0/0
```
