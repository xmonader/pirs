// spend-caps.rhai — persistent USD spending caps (daily/monthly), hydrated
// from ~/.pirs/spend.json on load, surviving restarts.

// USD per token, rough public prices — edit to taste.
const PRICES = #{
    "default": #{ input: 0.000001, output: 0.000003 }
};
const DAILY_USD = 2.0;
const MONTHLY_USD = 20.0;

fn spend_path() {
    let home = exec("printf %s \"$HOME\"").output;
    home.trim();
    `${home}/.pirs/spend.json`
}

fn today() {
    (now_millis() / 86_400_000).to_string()
}
fn month() {
    (now_millis() / 2_592_000_000).to_string()
}

fn load_spend() {
    let raw = fs_read(spend_path());
    if raw == "" {
        return #{};
    }
    raw.parse_json()
}

fn save_spend(data) {
    fs_write(spend_path(), data.to_json());
}

if !state_has("spend_loaded") {
    state_set("spend_loaded", true);
    let data = load_spend();
    state_set("day_spent", data[today()] ?? 0.0);
    state_set("month_spent", data[month()] ?? 0.0);
}

fn on_event(ty, data) {
    if ty != "turn_end" {
        return ();
    }
    let model = state_get("model") ?? "default";
    let p = if PRICES.contains(model) { PRICES[model] } else { PRICES["default"] };
    let cost = data.inputTokens * p.input + data.outputTokens * p.output;
    let day = (state_get("day_spent") ?? 0.0) + cost;
    let mon = (state_get("month_spent") ?? 0.0) + cost;
    state_set("day_spent", day);
    state_set("month_spent", mon);
    let data = load_spend();
    data[today()] = day;
    data[month()] = mon;
    save_spend(data);
}

fn on_should_stop(info) {
    let day = state_get("day_spent") ?? 0.0;
    let mon = state_get("month_spent") ?? 0.0;
    day >= DAILY_USD || mon >= MONTHLY_USD
}

fn on_steering() {
    let day = state_get("day_spent") ?? 0.0;
    if day >= DAILY_USD * 0.8 && !state_has("warned") {
        state_set("warned", true);
        return `Spend warning: $${day} today (cap $${DAILY_USD}). Wrap up.`;
    }
    ()
}
