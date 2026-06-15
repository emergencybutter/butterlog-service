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
