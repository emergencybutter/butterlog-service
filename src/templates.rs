//! Askama page templates. The HTML lives in `templates/`; these structs carry
//! the per-request data. Fields render with askama's HTML auto-escaping unless
//! the template marks them `|safe` (pre-rendered fragments and JSON blobs).

use askama::Template;

/// Navigation state for the `/content` application chrome. Every page rendered
/// inside the shell carries one, and `_app_shell.html` reads it directly.
pub struct Nav {
    /// `community` | `flights` | `aircraft` | `settings`. Drives the active mark.
    pub active: String,
    /// The current view's name, shown in the narrow-screen top bar.
    pub section: String,
    pub signed_in: bool,
    /// Where the two personal destinations point. Signed out, both send the
    /// visitor through the login flow rather than disappearing - a menu that
    /// changes shape depending on session state is harder to learn.
    pub my_flights_href: String,
    pub my_aircraft_href: String,
    pub avatar_url: String,
    pub display_name: String,
}

impl Nav {
    /// The signed-out rail: personal destinations route through login.
    pub fn anonymous(active: &str, section: &str) -> Self {
        Nav {
            active: active.to_string(),
            section: section.to_string(),
            signed_in: false,
            my_flights_href: "/api/v0/auth/login".to_string(),
            my_aircraft_href: "/api/v0/auth/login".to_string(),
            avatar_url: String::new(),
            display_name: String::new(),
        }
    }

    pub fn signed_in(active: &str, section: &str, user_id: i64, name: String, avatar: String) -> Self {
        Nav {
            active: active.to_string(),
            section: section.to_string(),
            signed_in: true,
            my_flights_href: format!("/content/flight/user/{}", user_id),
            my_aircraft_href: "/content/aircraft".to_string(),
            avatar_url: avatar,
            display_name: name,
        }
    }
}

#[derive(Template)]
#[template(path = "home.html")]
pub struct HomePage;

#[derive(Template)]
#[template(path = "map.html")]
pub struct MapPage {
    pub nav: Nav,
}

/// The live variant of the flight detail page. Carries only the flight id: the
/// page fetches the document itself from `/api/v0/flights/:id/live` and then
/// keeps it current, so there is nothing to server-render.
#[derive(Template)]
#[template(path = "live_detail.html")]
pub struct LiveDetailPage {
    pub nav: Nav,
    pub flight_id: i64,
    /// Only the pilot gets the control strip. Remote control of a running
    /// simulator is the one owner-only part of the live-flight feature.
    pub is_owner: bool,
}

#[derive(Template)]
#[template(path = "flight_detail.html")]
pub struct FlightDetailPage {
    pub nav: Nav,
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
    pub nav: Nav,
    /// Short context line beside the view title, e.g. "Every pilot".
    pub subtitle: String,
    pub flights: Vec<FlightCard>,
}

pub struct FlightCard {
    /// Link to the share page; empty when the flight has no share.
    pub share_href: String,
    /// Link to the live flight page; empty unless the flight is in progress and
    /// streaming a track. Takes precedence over `share_href`, which an
    /// in-progress flight does not have yet — without this the one card a
    /// viewer most wants to click is the only one that isn't clickable.
    pub live_href: String,
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
    pub nav: Nav,
    /// True when the boards are limited to the signed-in pilot's own flights.
    pub mine: bool,
    /// Set when the viewer can switch scope; empty hides the control.
    pub scope_all_href: String,
    pub scope_mine_href: String,
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
    /// The model name behind that code (e.g. `Airbus A320 Neo`); empty when the
    /// code is not in the characteristics table.
    pub name: String,
    /// Headline metric for this list, formatted (e.g. `9,877 nm`).
    pub value: String,
    /// The other two metrics, formatted as a single caption line.
    pub sub: String,
    /// Bar width as a whole percent of the list leader (0–100).
    pub pct: u32,
    /// Where the row leads: this type's screenshots, in the board's own scope.
    pub href: String,
}

/// Every screenshot taken in one aircraft type, newest first.
#[derive(Template)]
#[template(path = "aircraft_type.html")]
pub struct AircraftTypePage {
    pub nav: Nav,
    /// ICAO type designator the page is about.
    pub icao: String,
    /// Model name behind the code; empty when the code is unknown.
    pub name: String,
    /// True when limited to the signed-in pilot's own flights.
    pub mine: bool,
    /// Scope switch targets; empty hides the control, as on the boards.
    pub scope_all_href: String,
    pub scope_mine_href: String,
    pub shots: Vec<TypeShot>,
    /// JSON array of every URL on the page, for the lightbox.
    pub urls_json: String,
}

pub struct TypeShot {
    pub url: String,
    /// Where the shot leads: the flight it came from. Empty when that flight
    /// has neither a share nor a live page.
    pub flight_href: String,
    pub dep: String,
    pub arr: String,
    pub pilot: String,
    pub date_str: String,
}

#[derive(Template)]
#[template(path = "settings.html")]
pub struct SettingsPage {
    pub nav: Nav,
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
    pub nav: Nav,
    pub share_id: String,
    pub is_owner: bool,
    /// `MSFS`, `X-Plane`, or empty when the share predates the flights link.
    /// The renderer reads the track's raw pitch and roll through it.
    pub simulator: String,
    /// Share JSON with `</` and backslashes escaped for safe <script> embedding.
    pub json_escaped: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nav(active: &str) -> Nav {
        let section = match active {
            "flights" => "My Flights",
            "aircraft" => "My Aircrafts",
            "settings" => "Settings",
            "map" => "Live Map",
            _ => "Community",
        };
        Nav::signed_in(active, section, 7, "Pilot".into(), "https://example.invalid/a.png".into())
    }

    #[test]
    fn static_pages_render() {
        assert!(HomePage.render().unwrap().contains("ButterLog Backend"));
        assert!(MapPage { nav: nav("map") }.render().unwrap().contains("ButterLog Live Traffic Map"));
    }

    #[test]
    fn live_page_wires_its_flight_id_into_the_poller() {
        let html = LiveDetailPage { nav: nav("community"), flight_id: 4242, is_owner: false }.render().unwrap();
        assert!(html.contains("const FLIGHT_ID = 4242;"));
        // The shared renderer must be inlined, not just referenced: the live
        // page and the share page draw with the same code.
        assert!(html.contains("function renderFlightDoc("));
        assert!(html.contains("function renderFlight3D("));
        // The flight display is part of the live page for everyone, pilot or not.
        assert!(html.contains("id=\"pfd-mount\""));
        assert!(html.contains("window.renderPFD ="));
    }

    #[test]
    fn only_the_pilot_gets_the_sim_control_strip() {
        // Remote control of a running simulator is the one owner-only part of
        // the live-flight feature; a visitor must not even receive the markup.
        let visitor = LiveDetailPage { nav: nav("community"), flight_id: 1, is_owner: false }.render().unwrap();
        assert!(!visitor.contains("controls-mount"));
        assert!(!visitor.contains("Sim controls"));

        let owner = LiveDetailPage { nav: nav("community"), flight_id: 1, is_owner: true }.render().unwrap();
        assert!(owner.contains("controls-mount"));
        assert!(owner.contains("Sim controls"));
        // Pause is the stable control; the autopilot ones sit behind the beta
        // disclosure with their caveat spelled out.
        assert!(owner.contains("Pause sim"));
        assert!(owner.contains("beta-chip"));
        assert!(owner.contains("PMDG"));
    }

    /// The simulator reaches the page through an attribute rather than a JS
    /// string, because it comes from a client-submitted statistics blob and
    /// askama's escaping is HTML escaping.
    #[test]
    fn a_hostile_simulator_name_cannot_break_out_of_the_share_page() {
        let html = ShareDetailPage {
            nav: nav("community"),
            share_id: "abc-123".into(),
            is_owner: false,
            simulator: r#"" ; alert(1); //"#.into(),
            json_escaped: "{}".into(),
        }
        .render()
        .unwrap();
        // The quote that would close the attribute is escaped, so the payload
        // stays inert data. It is still *present* - escaped, not stripped.
        assert!(html.contains(r#"data-sim="&quot; ; alert(1); //""#), "attribute not escaped");
        // And the script reads it as data rather than being generated with it.
        assert!(html.contains("SHARE_DATA.simulator = document.currentScript.dataset.sim"));
    }

    #[test]
    fn share_page_uses_the_same_shared_renderer() {
        let html = ShareDetailPage {
            nav: nav("community"),
            share_id: "abc-123".into(),
            is_owner: false,
            simulator: "MSFS".into(),
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
            nav: nav("community"),
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
            nav: nav("community"),
            subtitle: "Every pilot".into(),
            flights: vec![FlightCard {
                share_href: String::new(),
                live_href: String::new(),
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
        // A flight with nowhere to go renders as a plain row, not a dead anchor.
        assert!(html.contains(r#"<div class="row">"#));
        // The rail is the navigation now, and it knows whose session this is.
        assert!(html.contains(r#"href="/content/flight/user/7""#));
        assert!(html.contains(r#"href="/content/aircraft""#));
    }

    /// A card links to whichever page actually exists for that flight, and an
    /// in-progress flight must not fall through to the non-clickable branch —
    /// it is the card a viewer is most likely to want.
    #[test]
    fn a_live_flight_card_links_to_the_live_page() {
        fn card(live: &str, share: &str) -> FlightCard {
            FlightCard {
                share_href: share.into(),
                live_href: live.into(),
                avatar_url: "a.png".into(),
                pilot: "Pilot".into(),
                dep: "EGLL".into(),
                arr: "In Flight".into(),
                airframe: "A320".into(),
                resolved_icao: String::new(),
                resolved_airline: String::new(),
                simulator: "MSFS".into(),
                date_str: "June 09, 2026, 12:00 UTC".into(),
                landing_badge: r#"<div class="badge badge-live">LIVE</div>"#.into(),
                screenshots: vec![],
                urls_json: "[]".into(),
            }
        }
        fn render(c: FlightCard) -> String {
            FlightsPage {
                nav: nav("community"),
                subtitle: "s".into(),
                flights: vec![c],
            }
            .render()
            .unwrap()
        }

        // In progress: links live, and is a real anchor rather than a dead div.
        let live = render(card("/content/flights/42", ""));
        assert!(live.contains(r#"href="/content/flights/42""#));
        assert!(!live.contains(r#"<div class="row">"#));
        assert!(live.contains("LIVE"));

        // A flight that is both live and already shared prefers the live page,
        // since that is the one still changing.
        let both = render(card("/content/flights/42", "/content/flights/share/abc"));
        assert!(both.contains(r#"href="/content/flights/42""#));
        assert!(!both.contains("/content/flights/share/abc"));

        // Finished and shared: unchanged behaviour.
        let done = render(card("", "/content/flights/share/abc"));
        assert!(done.contains(r#"href="/content/flights/share/abc""#));

        let empty = FlightsPage {
            nav: nav("community"),
            subtitle: "s".into(),
            flights: vec![],
        };
        let html = empty.render().unwrap();
        assert!(html.contains("No flights here yet"));
    }


    /// Dumps the shell pages to disk so the redesign can be looked at rather
    /// than only asserted about. Ignored by default; run with
    /// `cargo test dump_shell_pages -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn dump_shell_pages() {
        let out = std::env::var("SHELL_DUMP_DIR").expect("SHELL_DUMP_DIR");
        let mk = |dep: &str, arr: &str, ac: &str, icao: &str, air: &str, sim: &str, d: &str, badge: &str, shots: Vec<String>| FlightCard {
            share_href: "/content/flights/share/abc".into(),
            live_href: String::new(),
            avatar_url: "https://cdn.discordapp.com/embed/avatars/0.png".into(),
            pilot: "IronClaw737".into(),
            dep: dep.into(), arr: arr.into(), airframe: ac.into(),
            resolved_icao: icao.into(), resolved_airline: air.into(),
            simulator: sim.into(), date_str: d.into(),
            landing_badge: badge.into(),
            urls_json: "[\"https://placehold.co/600x400\"]".into(),
            screenshots: shots,
        };
        let shot = "https://placehold.co/120x80/1e2220/8a938c?text=+".to_string();
        let flights = vec![
            mk("VIDP", "Airborne", "A300 Freighter (GE)", "A306", "FedEx Express", "MSFS", "31 Aug 2026, 18:04 UTC", r#"<div class="badge badge-live">LIVE</div>"#, vec![]),
            mk("KMTH", "KMTH", "C185F Skywagon Tundra", "C185", "Yute Air Alaska", "MSFS", "18 Aug 2026, 04:24 UTC", r#"<div class="badge badge-butter">BUTTER<span class="badge-detail"> 36 fpm</span></div>"#, vec![shot.clone(), shot.clone(), shot.clone(), shot.clone()]),
            mk("EGLL", "LFPG", "A320neo", "A20N", "British Airways", "X-Plane", "12 Aug 2026, 09:11 UTC", r#"<div class="badge badge-firm">FIRM<span class="badge-detail"> 284 fpm</span></div>"#, vec![shot.clone()]),
            mk("EBOS", "EHAM", "Citation Longitude", "C68A", "Skyward", "MSFS", "02 Aug 2026, 15:40 UTC", r#"<div class="badge badge-ongoing">ONGOING</div>"#, vec![]),
        ];
        let page = FlightsPage { nav: nav("community"), subtitle: "Every pilot".into(), flights };
        std::fs::write(format!("{}/shell_community.html", out), page.render().unwrap()).unwrap();

        let rows = |n: usize| (1..=n).map(|i| StatRow {
            rank: i,
            icao: ["A20N", "C185", "B738", "A306", "C68A"][i - 1].into(),
            name: ["Airbus A320 Neo", "Cessna 185 Skywagon", "Boeing 737-800",
                   "Airbus A300-600", "Cessna 680A Citation Latitude"][i - 1].into(),
            value: format!("{} flights", 40 - i * 6),
            sub: format!("{:.1} h · {} nm", 61.0 - i as f64 * 8.0, 4200 - i * 500),
            pct: (100 - (i - 1) * 18) as u32,
            href: format!("/content/aircraft/{}", ["A20N", "C185", "B738", "A306", "C68A"][i - 1]),
        }).collect::<Vec<_>>();
        let stats = StatsPage {
            nav: nav("aircraft"), mine: true,
            scope_all_href: "/content/stats".into(),
            scope_mine_href: "/content/aircraft".into(),
            by_flights: rows(5), by_time: rows(5), by_distance: rows(5),
        };
        std::fs::write(format!("{}/shell_aircraft.html", out), stats.render().unwrap()).unwrap();

        let settings = SettingsPage {
            nav: nav("settings"),
            admin_guilds: vec![AdminGuild {
                name: "Voyager Aviation".into(),
                channels: vec![
                    AdminChannel { id: "1".into(), name: "flight-log".into(), js_name: "flight-log".into(), guild_id: "9".into(), checked: true },
                    AdminChannel { id: "2".into(), name: "landings".into(), js_name: "landings".into(), guild_id: "9".into(), checked: false },
                ],
            }],
            notified_guilds: vec![NotifiedGuild {
                name: "Voyager Aviation".into(),
                channels: vec![NotifiedChannel { id: "1".into(), name: "flight-log".into() }],
            }],
        };
        std::fs::write(format!("{}/shell_settings.html", out), settings.render().unwrap()).unwrap();

        std::fs::write(format!("{}/shell_map.html", out), MapPage { nav: nav("map") }.render().unwrap()).unwrap();

        let empty = FlightsPage { nav: nav("flights"), subtitle: "Single pilot".into(), flights: vec![] };
        std::fs::write(format!("{}/shell_empty.html", out), empty.render().unwrap()).unwrap();

        let shot = |dep: &str, arr: &str, who: &str, d: &str| TypeShot {
            url: "https://placehold.co/640x400/1e2220/8a938c?text=+".into(),
            flight_href: "/content/flights/share/abc".into(),
            dep: dep.into(), arr: arr.into(), pilot: who.into(), date_str: d.into(),
        };
        let shots = vec![
            shot("EGLL", "LFPG", "IronClaw737", "18 Aug 2026"),
            shot("KMTH", "KMTH", "IronClaw737", "17 Aug 2026"),
            shot("VIDP", "VNSI", "smallpigeye", "12 Aug 2026"),
            shot("EBOS", "EHAM", "IronClaw737", "02 Aug 2026"),
            shot("LSZH", "LSGG", "IronClaw737", "28 Jul 2026"),
        ];
        let urls: Vec<&str> = shots.iter().map(|s| s.url.as_str()).collect();
        let ty = AircraftTypePage {
            nav: nav("aircraft"),
            icao: "A20N".into(),
            name: "Airbus A320 Neo".into(),
            mine: true,
            scope_all_href: "/content/stats/A20N".into(),
            scope_mine_href: "/content/aircraft/A20N".into(),
            urls_json: serde_json::to_string(&urls).unwrap(),
            shots,
        };
        std::fs::write(format!("{}/shell_type.html", out), ty.render().unwrap()).unwrap();

        let share = ShareDetailPage {
            nav: nav("community"),
            share_id: "abc".into(),
            is_owner: true,
            simulator: "MSFS".into(),
            json_escaped: "{}".into(),
        }
        .render()
        .unwrap();
        std::fs::write(format!("{}/shell_share.html", out), share).unwrap();

    }

    #[test]
    fn settings_page_escapes_discord_names() {
        let page = SettingsPage {
            nav: nav("settings"),
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
            nav: nav("community"),
            share_id: "abc-123".into(),
            is_owner: true,
            simulator: "MSFS".into(),
            json_escaped: r#"{"summary":{"x":"<\/script>"}}"#.into(),
        };
        let html = owner.render().unwrap();
        assert!(html.contains("/api/v0/flights/share/abc-123"));
        assert!(html.contains(r#"const SHARE_DATA = {"summary":{"x":"<\/script>"}};"#));

        let visitor = ShareDetailPage {
            nav: nav("community"),
            share_id: "abc-123".into(),
            is_owner: false,
            simulator: "MSFS".into(),
            json_escaped: "{}".into(),
        };
        assert!(!visitor.render().unwrap().contains("Delete Share"));
    }
}
