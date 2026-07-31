//! Askama page templates. The HTML lives in `templates/`; these structs carry
//! the per-request data. Fields render with askama's HTML auto-escaping unless
//! the template marks them `|safe` (pre-rendered fragments and JSON blobs).

use askama::Template;

#[derive(Template)]
#[template(path = "home.html")]
pub struct HomePage;

#[derive(Template)]
#[template(path = "map.html")]
pub struct MapPage;

/// The live variant of the flight detail page. Carries only the flight id: the
/// page fetches the document itself from `/api/v0/flights/:id/live` and then
/// keeps it current, so there is nothing to server-render.
#[derive(Template)]
#[template(path = "live_detail.html")]
pub struct LiveDetailPage {
    pub flight_id: i64,
    /// Only the pilot gets the control strip. Remote control of a running
    /// simulator is the one owner-only part of the live-flight feature.
    pub is_owner: bool,
}

#[derive(Template)]
#[template(path = "flight_detail.html")]
pub struct FlightDetailPage {
    pub dep: String,
    pub arr_display: String,
    pub pilot: String,
    pub airframe: String,
    /// ICAO type designator deduced from the title/livery; empty hides the line.
    pub resolved_icao: String,
    /// Operating airline deduced from the title/livery; empty hides the line.
    pub resolved_airline: String,
    pub simulator: String,
    pub date_str: String,
    /// Pre-rendered badge HTML (server-controlled), empty when still airborne.
    pub landing_badge: String,
    /// Raw note text; empty hides the section. Escaped by the template.
    pub notes: String,
    pub screenshots: Vec<String>,
    /// JSON array of screenshot URLs for the lightbox onclick handler.
    pub urls_json: String,
    /// Touchdown telemetry (from `landing_snapshot`); empty hides the card.
    pub touchdown_stats: Vec<StatItem>,
    /// Peak-of-flight telemetry (from `max_entries`); empty hides the card.
    pub peak_stats: Vec<StatItem>,
    /// Relative link to this flight's public 3D share page; empty when unshared.
    pub share_href: String,
}

/// A single label/value row in a flight-detail stats card.
pub struct StatItem {
    pub label: String,
    pub value: String,
}

#[derive(Template)]
#[template(path = "flights.html")]
pub struct FlightsPage {
    pub subtitle: String,
    pub history_active: bool,
    pub show_my_flights: bool,
    pub my_flights_href: String,
    pub my_flights_active: bool,
    pub flights: Vec<FlightCard>,
}

pub struct FlightCard {
    /// Link to the share page; empty when the flight has no share.
    pub share_href: String,
    pub avatar_url: String,
    pub pilot: String,
    pub dep: String,
    pub arr: String,
    pub airframe: String,
    /// ICAO type designator deduced from the title/livery; empty hides the line.
    pub resolved_icao: String,
    /// Operating airline deduced from the title/livery; empty hides the line.
    pub resolved_airline: String,
    pub simulator: String,
    pub date_str: String,
    /// Pre-rendered badge HTML (landing rating or ONGOING).
    pub landing_badge: String,
    pub screenshots: Vec<String>,
    pub urls_json: String,
}

#[derive(Template)]
#[template(path = "stats.html")]
pub struct StatsPage {
    /// Aircraft ranked by number of logged flights.
    pub by_flights: Vec<StatRow>,
    /// Aircraft ranked by total flown time.
    pub by_time: Vec<StatRow>,
    /// Aircraft ranked by total great-circle distance.
    pub by_distance: Vec<StatRow>,
}

/// One row of an aircraft leaderboard, pre-formatted for display.
pub struct StatRow {
    /// 1-based placing within its list.
    pub rank: usize,
    /// ICAO type designator (e.g. `A320`).
    pub icao: String,
    /// Headline metric for this list, formatted (e.g. `9,877 nm`).
    pub value: String,
    /// The other two metrics, formatted as a single caption line.
    pub sub: String,
    /// Bar width as a whole percent of the list leader (0–100).
    pub pct: u32,
}

#[derive(Template)]
#[template(path = "settings.html")]
pub struct SettingsPage {
    /// Guilds where the logged-in user is an administrator.
    pub admin_guilds: Vec<AdminGuild>,
    /// Guilds with channels currently receiving this user's notifications.
    pub notified_guilds: Vec<NotifiedGuild>,
}

pub struct AdminGuild {
    pub name: String,
    pub channels: Vec<AdminChannel>,
}

pub struct AdminChannel {
    pub id: String,
    pub name: String,
    /// Name escaped for a single-quoted JS string inside the onclick attribute
    /// (HTML-escaped plus backslash-escaped quotes); rendered `|safe`.
    pub js_name: String,
    pub guild_id: String,
    pub checked: bool,
}

pub struct NotifiedGuild {
    pub name: String,
    pub channels: Vec<NotifiedChannel>,
}

pub struct NotifiedChannel {
    pub id: String,
    pub name: String,
}

#[derive(Template)]
#[template(path = "share_detail.html")]
pub struct ShareDetailPage {
    pub share_id: String,
    pub is_owner: bool,
    /// Share JSON with `</` and backslashes escaped for safe <script> embedding.
    pub json_escaped: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_pages_render() {
        assert!(HomePage.render().unwrap().contains("ButterLog Backend"));
        assert!(MapPage.render().unwrap().contains("ButterLog Live Traffic Map"));
    }

    #[test]
    fn live_page_wires_its_flight_id_into_the_poller() {
        let html = LiveDetailPage { flight_id: 4242, is_owner: false }.render().unwrap();
        assert!(html.contains("const FLIGHT_ID = 4242;"));
        // The shared renderer must be inlined, not just referenced: the live
        // page and the share page draw with the same code.
        assert!(html.contains("function renderFlightDoc("));
        assert!(html.contains("function renderFlight3D("));
    }

    #[test]
    fn only_the_pilot_gets_the_sim_control_strip() {
        // Remote control of a running simulator is the one owner-only part of
        // the live-flight feature; a visitor must not even receive the markup.
        let visitor = LiveDetailPage { flight_id: 1, is_owner: false }.render().unwrap();
        assert!(!visitor.contains("controls-mount"));
        assert!(!visitor.contains("Sim controls"));

        let owner = LiveDetailPage { flight_id: 1, is_owner: true }.render().unwrap();
        assert!(owner.contains("controls-mount"));
        assert!(owner.contains("Sim controls"));
        // Pause is the stable control; the autopilot ones sit behind the beta
        // disclosure with their caveat spelled out.
        assert!(owner.contains("Pause sim"));
        assert!(owner.contains("beta-chip"));
        assert!(owner.contains("PMDG"));
    }

    #[test]
    fn share_page_uses_the_same_shared_renderer() {
        let html = ShareDetailPage {
            share_id: "abc-123".into(),
            is_owner: false,
            json_escaped: "{}".into(),
        }
        .render()
        .unwrap();
        assert!(html.contains("function renderFlightDoc("));
        assert!(html.contains("renderFlightDoc(SHARE_DATA);"));
    }

    #[test]
    fn flight_detail_escapes_user_content() {
        let page = FlightDetailPage {
            dep: "KSFO".into(),
            arr_display: "KLAX".into(),
            pilot: "<script>alert(1)</script>".into(),
            airframe: "Cessna \"172\"".into(),
            resolved_icao: "C172".into(),
            resolved_airline: String::new(),
            simulator: "MSFS".into(),
            date_str: "June 09, 2026, 12:00 UTC".into(),
            landing_badge: r#"<div class="badge badge-butter">BUTTER</div>"#.into(),
            notes: "line1\n<b>not bold</b>".into(),
            screenshots: vec!["https://cdn.example/s/1.webp".into()],
            urls_json: r#"["https://cdn.example/s/1.webp"]"#.into(),
            touchdown_stats: vec![StatItem { label: "Vertical Speed".into(), value: "-121.00 fpm".into() }],
            peak_stats: vec![StatItem { label: "Indicated Airspeed".into(), value: "142.00 kts".into() }],
            share_href: "/content/flights/share/abc123".into(),
        };
        let html = page.render().unwrap();
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!html.contains("<script>alert(1)"));
        assert!(html.contains("Cessna &quot;172&quot;") || html.contains("Cessna &#34;172&#34;"));
        // Pre-rendered badge passes through unescaped
        assert!(html.contains(r#"<div class="badge badge-butter">BUTTER</div>"#));
        assert!(html.contains("&lt;b&gt;not bold&lt;/b&gt;"));
        assert!(html.contains(r#"openLightbox(["https://cdn.example/s/1.webp"], 0)"#));
        // Deduced type shows; the empty airline line is omitted.
        assert!(html.contains("C172"));
        // Stats cards and the 3D share link render.
        assert!(html.contains("Vertical Speed") && html.contains("-121.00 fpm"));
        assert!(html.contains("142.00 kts"));
        assert!(html.contains(r#"href="/content/flights/share/abc123""#));
    }

    #[test]
    fn flights_page_nav_and_cards() {
        let page = FlightsPage {
            subtitle: "Telemetry records from every pilot".into(),
            history_active: true,
            show_my_flights: true,
            my_flights_href: "/content/flight/user/7".into(),
            my_flights_active: false,
            flights: vec![FlightCard {
                share_href: String::new(),
                avatar_url: "https://cdn.discordapp.com/embed/avatars/0.png".into(),
                pilot: "Pilot".into(),
                dep: "EGLL".into(),
                arr: "In Flight".into(),
                airframe: "A320".into(),
                resolved_icao: "A320".into(),
                resolved_airline: "British Airways".into(),
                simulator: "X-Plane".into(),
                date_str: "June 09, 2026, 12:00 UTC".into(),
                landing_badge: r#"<div class="badge badge-ongoing">ONGOING</div>"#.into(),
                screenshots: vec![],
                urls_json: "[]".into(),
            }],
        };
        let html = page.render().unwrap();
        assert!(html.contains(r#"href="/content/flight/user/7""#));
        assert!(html.contains("ONGOING"));
        // Deduced type and airline render on the card.
        assert!(html.contains("British Airways"));
        // Unshared flights render as a non-link card
        assert!(html.contains(r#"<div class="flight-card-link" style="cursor:default">"#));

        let empty = FlightsPage {
            subtitle: "s".into(),
            history_active: true,
            show_my_flights: false,
            my_flights_href: String::new(),
            my_flights_active: false,
            flights: vec![],
        };
        let html = empty.render().unwrap();
        assert!(html.contains("No flights logged yet"));
        assert!(!html.contains("My Flights"));
    }

    #[test]
    fn settings_page_escapes_discord_names() {
        let page = SettingsPage {
            admin_guilds: vec![AdminGuild {
                name: "<img src=x onerror=alert(1)>".into(),
                channels: vec![AdminChannel {
                    id: "123".into(),
                    name: "it's-a-channel".into(),
                    js_name: "it\'s-a-channel".into(),
                    guild_id: "456".into(),
                    checked: true,
                }],
            }],
            notified_guilds: vec![],
        };
        let html = page.render().unwrap();
        assert!(!html.contains("<img src=x onerror"));
        assert!(html.contains("&lt;img src=x onerror=alert(1)&gt;"));
        // js_name renders raw (backslash-escaped for the JS string)
        assert!(html.contains("toggleAllowlist('123', '456', 'it\'s-a-channel', this.checked)"));
        assert!(html.contains("checked"));
        assert!(html.contains("No active notification channels found"));
    }

    #[test]
    fn share_page_owner_controls_and_json() {
        let owner = ShareDetailPage {
            share_id: "abc-123".into(),
            is_owner: true,
            json_escaped: r#"{"summary":{"x":"<\/script>"}}"#.into(),
        };
        let html = owner.render().unwrap();
        assert!(html.contains("/api/v0/flights/share/abc-123"));
        assert!(html.contains(r#"const SHARE_DATA = {"summary":{"x":"<\/script>"}};"#));

        let visitor = ShareDetailPage {
            share_id: "abc-123".into(),
            is_owner: false,
            json_escaped: "{}".into(),
        };
        assert!(!visitor.render().unwrap().contains("Delete Share"));
    }
}
