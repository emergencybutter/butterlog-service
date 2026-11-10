//! Askama page templates. The HTML lives in `templates/`; these structs carry
//! the per-request data. Fields render with askama's HTML auto-escaping unless
//! the template marks them `|safe` (pre-rendered fragments and JSON blobs).

use askama::Template;

/// Navigation state for the `/content` application chrome. Every page rendered
/// inside the shell carries one, and `_app_shell.html` reads it directly.
pub struct Nav {
    /// `flights` | `aircraft` | `map` | `settings`. Drives the active mark.
    pub active: String,
    /// The current view's name, shown in the narrow-screen top bar.
    pub section: String,
    pub signed_in: bool,
    /// Where Flights and Aircrafts open: the pilot's own, signed in, as each page's
    /// Mine; everyone's, signed out, who have no "mine" to show.
    pub flights_href: String,
    pub aircraft_href: String,
    pub avatar_url: String,
    pub display_name: String,
}

impl Nav {
    /// The signed-out rail: Flights and Aircrafts open on everyone's.
    pub fn anonymous(active: &str, section: &str) -> Self {
        Nav {
            active: active.to_string(),
            section: section.to_string(),
            signed_in: false,
            flights_href: "/content".to_string(),
            aircraft_href: "/content/stats".to_string(),
            avatar_url: String::new(),
            display_name: String::new(),
        }
    }

    pub fn signed_in(active: &str, section: &str, user_id: i64, name: String, avatar: String) -> Self {
        Nav {
            active: active.to_string(),
            section: section.to_string(),
            signed_in: true,
            flights_href: format!("/content/flight/user/{}", user_id),
            aircraft_href: "/content/aircraft".to_string(),
            avatar_url: avatar,
            display_name: name,
        }
    }
}

#[derive(Template)]
#[template(path = "home.html")]
pub struct HomePage;

/// The telemetry comparison: the pilot's flights as the service analysed them, beside the
/// app's (see `analysis::compare`).
#[derive(Template)]
#[template(path = "telemetry.html")]
pub struct TelemetryPage {
    pub nav: Nav,
    pub flights: Vec<crate::analysis::compare::FlightRow>,
    pub failures: Vec<crate::analysis::compare::FailedSession>,
}

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
    /// The FlySto export, for its pilot when its telemetry has it; empty hides it.
    pub export_href: String,
    /// Where its pilot sends it to FlySto, beside the export; empty hides it.
    pub upload_href: String,
    /// Whether its pilot has connected FlySto; without, the upload leads to Settings.
    pub flysto_connected: bool,
    /// Where it is on FlySto (`flysto::Pointer`): `none`, `processing`, `done` or `empty`.
    pub flysto_state: String,
    /// Its log on FlySto once there (`done`); empty before.
    pub flysto_url: String,
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
    /// Short context line beside the view title, e.g. "Flown by every pilot".
    pub subtitle: String,
    /// Which of the Mine / Everyone tabs this list is: `mine`, `everyone`, or empty
    /// for another pilot's, neither.
    pub scope: String,
    pub flights: Vec<FlightCard>,
}

pub struct FlightCard {
    /// The flight's page: every flight has one, live while it flies.
    pub href: String,
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
    /// One of those screenshots, the newest, shown in the row; empty without any.
    pub shot: String,
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
    /// Where the shot leads: the page of the flight it came from.
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
    /// Whether the pilot's FlySto API token is kept, for uploading flights there.
    pub flysto_connected: bool,
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
#[template(path = "flight_view.html")]
pub struct FlightViewPage {
    pub nav: Nav,
    /// The FlySto export, for the flight's pilot when its telemetry has it; empty hides it.
    pub export_href: String,
    /// Where its pilot sends it to FlySto, beside the export; empty hides it.
    pub upload_href: String,
    /// Whether its pilot has connected FlySto; without, the upload leads to Settings.
    pub flysto_connected: bool,
    /// Where it is on FlySto (`flysto::Pointer`): `none`, `processing`, `done` or `empty`.
    pub flysto_state: String,
    /// Its log on FlySto once there (`done`); empty before.
    pub flysto_url: String,
    /// `MSFS`, `X-Plane`, or empty when unknown (an old share without its flight).
    /// The renderer reads the track's raw pitch and roll through it.
    pub simulator: String,
    /// The flight's document as JSON, with no `<` left to close the script it is in.
    pub json_escaped: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nav(active: &str) -> Nav {
        let section = match active {
            "aircraft" => "Aircrafts",
            "settings" => "Settings",
            "map" => "Live Map",
            _ => "Flights",
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
        let html = LiveDetailPage { nav: nav("flights"), flight_id: 4242, is_owner: false }.render().unwrap();
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
        let visitor = LiveDetailPage { nav: nav("flights"), flight_id: 1, is_owner: false }.render().unwrap();
        assert!(!visitor.contains("controls-mount"));
        assert!(!visitor.contains("Sim controls"));

        let owner = LiveDetailPage { nav: nav("flights"), flight_id: 1, is_owner: true }.render().unwrap();
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
        let html = FlightViewPage {
            nav: nav("flights"),
            export_href: String::new(),
            upload_href: String::new(),
            flysto_connected: false,
            flysto_state: "none".into(),
            flysto_url: String::new(),
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
        let html = FlightViewPage {
            nav: nav("flights"),
            export_href: String::new(),
            upload_href: String::new(),
            flysto_connected: false,
            flysto_state: "none".into(),
            flysto_url: String::new(),
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
            nav: nav("flights"),
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
            export_href: "/content/flights/7/export/flysto.csv".into(),
            upload_href: "/content/flights/7/upload/flysto".into(),
            flysto_connected: true,
            flysto_state: "none".into(),
            flysto_url: String::new(),
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
        // Stats cards and the export render.
        assert!(html.contains("Vertical Speed") && html.contains("-121.00 fpm"));
        assert!(html.contains("142.00 kts"));
        assert!(html.contains(r#"href="/content/flights/7/export/flysto.csv""#));
    }

    #[test]
    fn flights_page_nav_and_cards() {
        let page = FlightsPage {
            nav: nav("flights"),
            subtitle: "Flown by every pilot".into(),
            scope: "everyone".into(),
            flights: vec![FlightCard {
                href: "/content/flights/9".into(),
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
        // Every flight leads to its page.
        assert!(html.contains(r#"<a class="row" href="/content/flights/9">"#));
        // The rail is the navigation now, and it knows whose session this is.
        assert!(html.contains(r#"href="/content/flight/user/7""#));
        assert!(html.contains(r#"href="/content/aircraft""#));
        // One Flights entry, with the Mine / Everyone switch on this list's tab.
        assert!(html.contains("<span>Flights</span>") && !html.contains("Community"));
        assert!(html.contains(r#"<a href="/content" class="is-on">Everyone</a>"#));
        assert!(html.contains(r#"<a href="/content/flight/user/7" class="">Mine</a>"#));
        // Signed out: no "mine" to switch to, and the rail opens everyone's.
        let html = FlightsPage { nav: Nav::anonymous("flights", "Flights"), ..page }.render().unwrap();
        assert!(!html.contains(">Mine</a>"));
        assert!(html.contains(r#"href="/content""#) && html.contains(r#"href="/content/stats""#));
    }

    #[test]
    fn an_empty_list_says_so() {
        let empty = FlightsPage {
            nav: nav("flights"),
            subtitle: "s".into(),
            scope: String::new(),
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
            href: "/content/flights/9".into(),
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
        let page = FlightsPage { nav: nav("flights"), subtitle: "Flown by every pilot".into(), scope: "everyone".into(), flights };
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
            // Not every type has a screenshot.
            shot: if i == 3 { String::new() } else { shot.clone() },
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
            flysto_connected: false,
        };
        std::fs::write(format!("{}/shell_settings.html", out), settings.render().unwrap()).unwrap();

        std::fs::write(format!("{}/shell_map.html", out), MapPage { nav: nav("map") }.render().unwrap()).unwrap();

        let empty = FlightsPage { nav: nav("flights"), subtitle: "Flown by you".into(), scope: "mine".into(), flights: vec![] };
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

        let share = FlightViewPage {
            nav: nav("flights"),
            export_href: String::new(),
            upload_href: String::new(),
            flysto_connected: false,
            flysto_state: "none".into(),
            flysto_url: String::new(),
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
            flysto_connected: true,
        };
        let html = page.render().unwrap();
        assert!(!html.contains("<img src=x onerror"));
        // Connected to FlySto: it says so, and offers to disconnect, never the token.
        assert!(html.contains(r#"id="flysto""#) && html.contains("Disconnect"));
        // What a toggle or Connect came to has somewhere to show.
        assert!(html.contains(r#"id="toast-container""#));
        assert!(html.contains("&lt;img src=x onerror=alert(1)&gt;"));
        // js_name renders raw (backslash-escaped for the JS string)
        assert!(html.contains("toggleAllowlist('123', '456', 'it\'s-a-channel', this.checked)"));
        assert!(html.contains("checked"));
        assert!(html.contains("No active notification channels found"));
    }

    #[test]
    fn flight_view_owner_export_and_json() {
        let owner = FlightViewPage {
            nav: nav("flights"),
            export_href: "/content/flights/7/export/flysto.csv".into(),
            upload_href: "/content/flights/7/upload/flysto".into(),
            flysto_connected: true,
            flysto_state: "none".into(),
            flysto_url: String::new(),
            simulator: "MSFS".into(),
            json_escaped: r#"{"summary":{"x":"</script>"}}"#.into(),
        };
        let html = owner.render().unwrap();
        assert!(html.contains(r#"href="/content/flights/7/export/flysto.csv""#));
        assert!(html.contains(r#"const SHARE_DATA = {"summary":{"x":"</script>"}};"#));
        assert!(!html.contains("Delete share"));
        assert!(html.contains(r#"data-flysto-upload="/content/flights/7/upload/flysto""#));
        // Already on FlySto: the page links its log rather than offering it again.
        let on_flysto = FlightViewPage {
            flysto_state: "done".into(),
            flysto_url: "https://www.flysto.net/logs/y6wg6irz".into(),
            ..owner
        }
        .render()
        .unwrap();
        assert!(on_flysto.contains(r#"href="https://www.flysto.net/logs/y6wg6irz""#) && on_flysto.contains("Open on FlySto"));
        assert!(!on_flysto.contains(r#"data-flysto-upload=""#));

        let visitor = FlightViewPage {
            nav: nav("flights"),
            export_href: String::new(),
            upload_href: String::new(),
            flysto_connected: false,
            flysto_state: "none".into(),
            flysto_url: String::new(),
            simulator: "MSFS".into(),
            json_escaped: "{}".into(),
        };
        let html = visitor.render().unwrap();
        assert!(!html.contains("Delete Share"));
        assert!(!html.contains("flysto.csv"));
    }
}
