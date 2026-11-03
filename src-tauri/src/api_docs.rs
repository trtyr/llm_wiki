//! Human-readable API documentation page, rendered server-side from the
//! OpenAPI document. Served at `GET /docs?token=...` so API consumers can
//! browse the surface in a browser instead of reading raw JSON. Fully
//! self-contained (inline CSS, no CDN) — works on offline tailnet hosts.

use serde_json::Value;

pub fn render(spec: &Value) -> String {
    let title = spec["info"]["title"].as_str().unwrap_or("API");
    let version = spec["info"]["version"].as_str().unwrap_or("");
    let mut out = String::with_capacity(128 * 1024);

    out.push_str("<!doctype html>\n<html lang=\"zh-CN\">\n<head>\n<meta charset=\"utf-8\">\n");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    out.push_str("<title>");
    push_escaped(&mut out, title);
    let title_suffix = if title.ends_with("API") {
        " 文档"
    } else {
        " API 文档"
    };
    out.push_str(title_suffix);
    out.push_str("</title>\n<style>\n");
    out.push_str(
        "body{font-family:system-ui,-apple-system,'Segoe UI',sans-serif;background:#fafafa;color:#1f2328;margin:0;padding:2rem 1rem;line-height:1.6}\n\
         .wrap{max-width:920px;margin:0 auto}\n\
         h1{font-size:1.7rem;border-bottom:2px solid #d0d7de;padding-bottom:.5rem}\n\
         h2{font-size:1.05rem;margin-top:0}\n\
         .ver{color:#59636e;font-weight:400;font-size:.9rem}\n\
         .notice{background:#fff8c6;border:1px solid #d4a72c;border-radius:8px;padding:.9rem 1rem;font-size:.92rem}\n\
         .ep{background:#fff;border:1px solid #d0d7de;border-radius:10px;padding:1rem 1.2rem;margin:1rem 0}\n\
         .method{display:inline-block;min-width:3.6rem;text-align:center;font-weight:700;font-size:.8rem;color:#fff;border-radius:5px;padding:.15rem .5rem;margin-right:.5rem}\n\
         .m-get{background:#1a7f37}.m-post{background:#0969da}.m-put{background:#8250df}.m-patch{background:#bf8700}.m-delete{background:#cf222e}\n\
         .path{font-family:ui-monospace,Consolas,monospace;font-weight:600;font-size:.95rem}\n\
         .sum{color:#59636e;font-size:.9rem;margin:.3rem 0 .5rem}\n\
         pre{background:#0d1117;color:#e6edf3;border-radius:8px;padding:.8rem 1rem;overflow-x:auto;font-size:.85rem}\n\
         code{font-family:ui-monospace,Consolas,monospace}\n\
         table{border-collapse:collapse;width:100%;font-size:.85rem;margin:.5rem 0}\n\
         th,td{border:1px solid #d0d7de;padding:.35rem .6rem;text-align:left;vertical-align:top}\n\
         th{background:#f6f8fa}\n\
         .req{color:#cf222e;font-weight:600}\n\
         section.auth{background:#ddf4ff;border:1px solid #54aeff}\n\
         code.inline{background:#eff2f5;border-radius:4px;padding:.1rem .35rem}\n\
         footer{color:#59636e;font-size:.85rem;margin-top:2rem}\n",
    );
    out.push_str("</style>\n</head>\n<body>\n<div class=\"wrap\">\n");
    out.push_str("<h1>");
    push_escaped(&mut out, title);
    out.push_str(&format!(
        "{}<span class=\"ver\">v{version}</span></h1>\n",
        title_suffix
    ));

    out.push_str("<section class=\"auth ep\">\n<h2>认证</h2>\n<p>除 <code class=\"inline\">/health</code> 与本页外，所有接口都需要 Bearer Token：\n请求头 <code class=\"inline\">Authorization: Bearer &lt;token&gt;</code>；浏览器访问可在 URL 后追加 <code class=\"inline\">?token=&lt;token&gt;</code>（本页即如此）。</p>\n");
    out.push_str("<p>机器对接请使用机器可读规范：<code class=\"inline\">GET /openapi.json</code>（OpenAPI 3.1，无需认证）。\n下方每个接口都附可直接修改的 curl 示例。</p>\n</section>\n");

    // Mirror the actual bind port so the curl examples work on deployments
    // that override LLM_WIKI_BIND_PORT.
    let sample_port = std::env::var("LLM_WIKI_BIND_PORT").unwrap_or_else(|_| "19828".to_string());
    let sample_base = format!("http://<host>:{sample_port}");
    if let Some(paths) = spec["paths"].as_object() {
        for (path, path_item) in paths {
            let Some(path_obj) = path_item.as_object() else {
                continue;
            };
            for (method, op) in path_obj {
                let method_lower = method.as_str();
                if !matches!(method_lower, "get" | "post" | "put" | "patch" | "delete") {
                    continue;
                }
                let summary = op["summary"].as_str().unwrap_or("");
                let description = op["description"].as_str().unwrap_or("");
                out.push_str("<section class=\"ep\">\n");
                out.push_str(&format!(
                    "<h2><span class=\"method m-{method_lower}\">{}</span><span class=\"path\">{}</span></h2>\n",
                    method_lower.to_uppercase(),
                    html_escape(path),
                ));
                if !summary.is_empty() {
                    out.push_str("<div class=\"sum\">");
                    push_escaped(&mut out, summary);
                    out.push_str("</div>\n");
                }
                if !description.is_empty() {
                    out.push_str("<p>");
                    push_escaped(&mut out, description);
                    out.push_str("</p>\n");
                }

                let parameters = op["parameters"].as_array();
                if let Some(params) = parameters {
                    if !params.is_empty() {
                        out.push_str("<table><tr><th>参数</th><th>位置</th><th>类型</th><th>必填</th><th>说明</th></tr>\n");
                        for param in params {
                            let name = param["name"].as_str().unwrap_or("");
                            let location = param["in"].as_str().unwrap_or("");
                            let required = param["required"].as_bool().unwrap_or(false);
                            let ptype = param["schema"]["type"].as_str().unwrap_or("string");
                            let desc = param["description"].as_str().unwrap_or("");
                            out.push_str("<tr><td><code class=\"inline\">");
                            push_escaped(&mut out, name);
                            out.push_str("</code></td><td>");
                            push_escaped(&mut out, location);
                            out.push_str("</td><td>");
                            push_escaped(&mut out, ptype);
                            out.push_str("</td><td>");
                            if required {
                                out.push_str("<span class=\"req\">必填</span>");
                            } else {
                                out.push_str("否");
                            }
                            out.push_str("</td><td>");
                            push_escaped(&mut out, desc);
                            out.push_str("</td></tr>\n");
                        }
                        out.push_str("</table>\n");
                    }
                }

                let curl = curl_example(&sample_base, path, method_lower, op);
                out.push_str("<pre><code>");
                push_escaped(&mut out, &curl);
                out.push_str("</code></pre>\n</section>\n");
            }
        }
    }

    out.push_str("<footer>由服务端从 OpenAPI 规范实时渲染 · 机器可读规范见 <code class=\"inline\">/openapi.json</code></footer>\n</div>\n</body>\n</html>\n");
    out
}

fn curl_example(base: &str, path: &str, method: &str, op: &Value) -> String {
    let mut query = Vec::new();
    if let Some(params) = op["parameters"].as_array() {
        for param in params {
            let in_query = param["in"].as_str() == Some("query");
            let required = param["required"].as_bool().unwrap_or(false);
            if in_query && required {
                let name = param["name"].as_str().unwrap_or("");
                query.push(format!("{name}=<{name}>"));
            }
        }
    }
    let query_string = if query.is_empty() {
        String::new()
    } else {
        format!("?{}", query.join("&"))
    };

    let body = op["requestBody"]["content"]["application/json"]["schema"].clone();
    let has_body = body.is_object();

    let mut lines = Vec::new();
    let method_flag = if method == "get" {
        ""
    } else {
        &format!("-X {} ", method.to_uppercase())
    };
    let auth = "-H \"Authorization: Bearer $TOKEN\"";
    let head = format!("curl {method_flag}{auth} \\\n  \"{base}{path}{query_string}\"");
    if has_body {
        let sample = sample_from_schema(&body);
        let compact = serde_json::to_string(&sample).unwrap_or_default();
        lines.push(head);
        lines.push("  -H \"Content-Type: application/json\" \\".to_string());
        lines.push(format!("  -d '{}'", compact.replace('\'', "'\\''")));
    } else if !method_flag.is_empty() {
        lines.push(head);
    } else {
        return head;
    }
    lines.join(" \\\n  ")
}

fn sample_from_schema(schema: &Value) -> Value {
    if let Some(example) = schema.get("example") {
        return example.clone();
    }
    let schema_type = schema["type"].as_str().unwrap_or("");
    if let Some(properties) = schema["properties"].as_object() {
        let mut map = serde_json::Map::new();
        for (key, prop_schema) in properties {
            map.insert(key.clone(), sample_from_schema(prop_schema));
        }
        return Value::Object(map);
    }
    match schema_type {
        "object" => Value::Object(serde_json::Map::new()),
        "array" => {
            let item = sample_from_schema(&schema["items"]);
            Value::Array(vec![item])
        }
        "integer" | "number" => serde_json::json!(0),
        "boolean" => serde_json::json!(false),
        _ => serde_json::json!("..."),
    }
}

fn push_escaped(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
}

fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    push_escaped(&mut out, text);
    out
}
