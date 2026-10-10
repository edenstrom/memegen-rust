//! OpenAPI document and Scalar API reference page.

use serde_json::{Value, json};

pub const DOCS_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Memegen API</title>
  <link rel="icon" href="/favicon.ico" sizes="32x32" />
  <link rel="icon" href="/favicon.svg" type="image/svg+xml" />
</head>
<body>
  <div id="app"></div>
  <script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference@1" crossorigin></script>
  <script>
    Scalar.createApiReference("#app", {
      url: "/openapi.json",
      showDeveloperTools: "never",
      agent: { disabled: true },
      mcp: { disabled: true },
    });
  </script>
</body>
</html>
"##;

fn query(name: &str, kind: &str, description: &str) -> Value {
    json!({ "name": name, "in": "query", "required": false, "schema": { "type": kind }, "description": description })
}

fn path(name: &str, description: &str) -> Value {
    json!({ "name": name, "in": "path", "required": true, "schema": { "type": "string" }, "description": description })
}

fn json_response(description: &str, schema: Value) -> Value {
    json!({ "description": description, "content": { "application/json": { "schema": schema } } })
}

fn image_response(description: &str) -> Value {
    let binary = json!({ "schema": { "type": "string", "format": "binary" } });
    json!({ "description": description, "content": { "image/*": binary, "video/mp4": binary } })
}

pub fn spec(base_url: &str) -> Value {
    let error = json!({ "$ref": "#/components/schemas/Error" });
    let meme = json!({ "$ref": "#/components/schemas/MemeResponse" });
    let request_body = |required: &[&str]| {
        json!({
            "content": {
                "application/json": { "schema": {
                    "$ref": if required.is_empty() { "#/components/schemas/MemeTemplateRequest" } else { "#/components/schemas/MemeRequest" }
                } },
                "application/x-www-form-urlencoded": { "schema": {
                    "$ref": if required.is_empty() { "#/components/schemas/MemeTemplateRequest" } else { "#/components/schemas/MemeRequest" }
                } }
            }
        })
    };
    let font = query("font", "string", "Font ID or alias (`GET /fonts/`)");
    let animate_text = query(
        "animate_text",
        "boolean",
        "Reveal the text boxes one after another, typed out a character at a time (gif, webp) or eased in (mp4), then hold the finished meme (gif, webp and mp4 only)",
    );

    json!({
        "openapi": "3.0.3",
        "info": {
            "title": "Memegen",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "High-performance Rust port of memegen.link. An unauthenticated MCP endpoint is available at `/mcp`."
        },
        "servers": [{ "url": base_url }],
        "paths": {
            "/templates/": { "get": {
                "tags": ["Templates"], "summary": "List all templates",
                "parameters": [
                    query("animated", "boolean", "Limit results to templates supporting animation"),
                    query("filter", "string", "Words to search for in the name, keywords, description, or example; results are ranked best match first"),
                ],
                "responses": { "200": json_response("Successfully returned a list of all templates", json!({ "type": "array", "items": { "$ref": "#/components/schemas/Template" } })) }
            } },
            "/templates/{id}": {
                "get": {
                    "tags": ["Templates"], "summary": "View a specific template",
                    "parameters": [path("id", "ID of a meme template")],
                    "responses": {
                        "200": json_response("Successfully returned a specific template", json!({ "$ref": "#/components/schemas/Template" })),
                        "404": json_response("Template not found", error.clone())
                    }
                },
                "post": {
                    "tags": ["Templates"], "summary": "Create a meme from a template",
                    "parameters": [path("id", "ID of a meme template")],
                    "requestBody": request_body(&[]),
                    "responses": {
                        "201": json_response("Successfully created a meme from a template", meme.clone()),
                        "404": json_response("Template not found", meme.clone())
                    }
                }
            },
            "/images/": {
                "get": {
                    "tags": ["Images"], "summary": "List example memes",
                    "parameters": [
                        query("filter", "string", "Words to search for in the template name, keywords, description, or example; results are ranked best match first"),
                        query("animated", "boolean", "Limit results to animated templates"),
                    ],
                    "responses": { "200": json_response("Successfully returned a list of example memes", json!({ "type": "array", "items": { "$ref": "#/components/schemas/Example" } })) }
                },
                "post": {
                    "tags": ["Images"], "summary": "Create a meme from a template",
                    "requestBody": request_body(&["template_id"]),
                    "responses": {
                        "201": json_response("Successfully created a meme", meme.clone()),
                        "400": json_response("Required \"template_id\" missing in request body", error.clone()),
                        "404": json_response("Specified \"template_id\" does not exist", meme.clone())
                    }
                }
            },
            "/images/{template_filename}": { "get": {
                "tags": ["Images"], "summary": "Display a template background",
                "parameters": [path("template_filename", "Template ID and image extension, e.g. `fry.png`"), font.clone()],
                "responses": {
                    "200": image_response("Successfully displayed a template background"),
                    "404": json_response("Template not found", error.clone()),
                    "422": json_response("Invalid extension or font", error.clone())
                }
            } },
            "/images/{template_id}/{text_filepath}": { "get": {
                "tags": ["Images"], "summary": "Display a custom meme",
                "description": "Lines are separated by `/`. Escapes: `_` space, `__` underscore, `--` dash, `~q` ?, `~a` &, `~p` %, `~h` #, `~s` /, `~b` \\, `~l` <, `~g` >, `~n` newline, `''` double quote. Use `:alias:` for emoji.",
                "parameters": [
                    path("template_id", "ID of a meme template"),
                    path("text_filepath", "Lines of text and image extension: `<line1>/<line2>.<png|jpg|gif|webp|mp4>`"),
                    font,
                    animate_text,
                ],
                "responses": {
                    "200": image_response("Successfully displayed a custom meme"),
                    "301": { "description": "Redirect to the canonical (normalized) URL" },
                    "404": json_response("Template not found", error.clone()),
                    "414": json_response("Custom text too long (length >200)", error.clone()),
                    "422": json_response("Invalid extension or font, or animated text with png/jpg", error.clone())
                }
            } },
            "/fonts/": { "get": {
                "tags": ["Fonts"], "summary": "List available fonts",
                "responses": { "200": json_response("Successfully returned a list of fonts", json!({ "type": "array", "items": { "$ref": "#/components/schemas/Font" } })) }
            } },
            "/fonts/{id}": { "get": {
                "tags": ["Fonts"], "summary": "View a specific font",
                "parameters": [path("id", "ID or alias of a font")],
                "responses": {
                    "200": json_response("Successfully returned a specific font", json!({ "$ref": "#/components/schemas/Font" })),
                    "404": json_response("Font not found", error.clone())
                }
            } }
        },
        "components": { "schemas": {
            "Error": { "type": "object", "properties": { "error": { "type": "string" } } },
            "MemeResponse": { "type": "object", "properties": { "url": { "type": "string" } } },
            "Example": { "type": "object", "properties": { "url": { "type": "string" }, "template": { "type": "string" } } },
            "MemeTemplateRequest": { "type": "object", "properties": {
                "text": { "type": "array", "items": { "type": "string" }, "description": "Lines of text (raw, not escape-encoded)" },
                "font": { "type": "string" },
                "extension": { "type": "string", "enum": ["png", "jpg", "jpeg", "gif", "webp", "mp4"] },
                "animate_text": { "type": "boolean", "description": "Reveal the text boxes one after another: typed out (gif, webp) or eased in (mp4). Defaults the extension to gif" },
                "redirect": { "type": "boolean", "description": "Redirect to the image instead of returning JSON" }
            } },
            "MemeRequest": { "allOf": [
                { "type": "object", "required": ["template_id"], "properties": { "template_id": { "type": "string" } } },
                { "$ref": "#/components/schemas/MemeTemplateRequest" }
            ] },
            "Font": { "type": "object", "properties": {
                "id": { "type": "string" }, "alias": { "type": "string", "nullable": true },
                "filename": { "type": "string" }, "_self": { "type": "string" }
            } },
            "Template": { "type": "object", "properties": {
                "id": { "type": "string" }, "name": { "type": "string" },
                "description": { "type": "string", "nullable": true },
                "lines": { "type": "integer" }, "overlays": { "type": "integer" },
                "styles": { "type": "array", "items": { "type": "string" } },
                "blank": { "type": "string" },
                "example": { "type": "object", "properties": {
                    "text": { "type": "array", "items": { "type": "string" } }, "url": { "type": "string" }
                } },
                "source": { "type": "string", "nullable": true },
                "keywords": { "type": "array", "items": { "type": "string" } },
                "_self": { "type": "string" }
            } }
        } }
    })
}
