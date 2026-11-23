use std::io::Write;
use std::process::{Command, Stdio};

const ADAPTER: &str = include_str!("../../../runtime/marimo/adapter.py");

fn interactive(bundles: &[serde_json::Value]) -> Option<Vec<bool>> {
    let start = ADAPTER.find("# What makes HTML interactive").unwrap();
    let end = ADAPTER.find("def html_text(").unwrap();
    let script = format!(
        "import json, re, sys\n{}\nfor line in sys.stdin.read().splitlines():\n    print(interactive_bundle(json.loads(line)))\n",
        &ADAPTER[start..end]
    );
    let works = Command::new("python3").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    if !works {
        return None;
    }
    let mut python = Command::new("python3").args(["-I", "-c", &script]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().ok()?;
    let input: Vec<String> = bundles.iter().map(|b| b.to_string()).collect();
    python.stdin.take().unwrap().write_all(input.join("\n").as_bytes()).unwrap();
    let out = python.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    Some(String::from_utf8(out.stdout).unwrap().lines().map(|l| l == "True").collect())
}

#[test]
fn plots_and_controls_are_interactive_and_tables_math_and_pictures_arent() {
    use serde_json::json;
    let cases = [
        ("plotly", json!({ "text/html": "<iframe srcdoc='&lt;div style=&quot;height:100%;&quot;&gt;&lt;script&gt;window.PlotlyConfig = {}&lt;/script&gt;'></iframe>" }), true),
        ("altair", json!({ "application/vnd.vegalite.v6+json": "{\"$schema\": \"https://vega.github.io/schema/vega-lite/v6.4.1.json\"}" }), true),
        ("mo.ui.slider", json!({ "text/html": "<marimo-ui-element object-id='e1' random-id='e1'><marimo-slider data-initial-value='1'></marimo-slider></marimo-ui-element>" }), true),
        ("mo.ui.plotly", json!({ "text/html": "<marimo-ui-element object-id='23' random-id='23'><marimo-plotly data-initial-value='{}'></marimo-plotly></marimo-ui-element>" }), true),
        ("mo.Html with a script", json!({ "text/html": "<div>a</div><script>1</script>" }), true),
        ("mo.md with a slider", json!({ "text/markdown": "<span class=\"markdown\"><marimo-ui-element object-id='1'><marimo-slider></marimo-slider></marimo-ui-element></span>" }), true),
        ("a table", json!({ "text/html": "<marimo-ui-element object-id='48' random-id='48'><marimo-table data-initial-value='[]' data-data='[]'></marimo-table></marimo-ui-element>" }), false),
        ("mo.md with math", json!({ "text/markdown": "<span class=\"markdown prose\"><span class=\"paragraph\">hi <marimo-tex class=\"arithmatex\">||(x||)</marimo-tex> <strong>b</strong></span></span>" }), false),
        ("an accordion of markdown", json!({ "text/html": "<marimo-accordion data-labels='[&quot;A&quot;]' data-multiple='false' data-expanded='[]'><div><span class=\"markdown prose\"><span class=\"paragraph\"><strong>hi</strong></span></span></div></marimo-accordion>" }), false),
        ("an accordion with a slider", json!({ "text/html": "<marimo-accordion data-labels='[&quot;A&quot;]' data-multiple='false' data-expanded='[]'><div><marimo-ui-element object-id='11' random-id='11'><marimo-slider data-initial-value='1'></marimo-slider></marimo-ui-element></div></marimo-accordion>" }), true),
        ("tabs of markdown", json!({ "text/html": "<marimo-ui-element object-id='cf' random-id='cf'><marimo-tabs data-initial-value='&quot;&quot;' data-tabs='[&quot;a&quot;]'><div data-kind='tab'><span class=\"markdown\">x</span></div></marimo-tabs></marimo-ui-element>" }), false),
        ("mo.lazy", json!({ "text/html": "<marimo-ui-element object-id='ca' random-id='ca'><marimo-lazy data-initial-value='false' data-label='null' data-show-loading-indicator='false'></marimo-lazy></marimo-ui-element>" }), true),
        ("a matplotlib picture", json!({ "image/png": "data:image/png;base64,iVBORw0KGgo=" }), false),
        ("plain text", json!({ "text/plain": "<script> is a word here" }), false),
        ("no output", json!({}), false),
    ];
    let bundles: Vec<_> = cases.iter().map(|(_, b, _)| b.clone()).collect();
    let Some(found) = interactive(&bundles) else {
        eprintln!("no python3: skipped");
        return;
    };
    for ((name, _, want), got) in cases.iter().zip(&found) {
        assert_eq!(got, want, "{name}");
    }
}
