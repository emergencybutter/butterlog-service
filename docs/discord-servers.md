# Posting flights in a Discord server

ButterLog's bot posts each pilot's flights in Discord. The post appears when the flight starts,
is edited as the flight goes, and ends as a landing card with the debrief and screenshots. Any
server can have it: its admins add the bot and choose the channels, and from then on every
member of the server who flies with ButterLog is posted there.

## For a server's admins

1. **Add the bot.** Open <https://butterlog.flyvoyager.net/content/discord/invite>, or press
   **Add ButterLog to a server** in ButterLog's settings. Pick the server and authorize.
   Discord only lists servers where you are the owner or have **Manage Server**.
   The bot asks for four permissions, which it needs in every channel it posts to:
   **View Channel**, **Send Messages**, **Embed Links** and **Attach Files** (for screenshots).
2. **Choose the channels.** Sign in at <https://butterlog.flyvoyager.net/content/settings> with
   Discord. Under **Admin Controls**, each server the bot is in lists its text and announcement
   channels. Tick one to post flights there. Untick it to stop.
   Choosing channels takes more than Manage Server: you must be the server's **owner** or hold
   the **Administrator** permission. Without either, the server is not listed.
3. **If ticking fails,** the message says what the bot lacks in that channel. Usually a channel
   hides itself from @everyone and doesn't let the bot's role back in. Grant the missing
   permissions to the ButterLog role, or to the bot in the channel's permissions, and tick
   again.

If the channel is deleted, ButterLog forgets it the next time it tries to post there. To take
the bot out of a server, untick its channels first, then kick it.

## For pilots

There is nothing to sign up for. Sign in to ButterLog with the same Discord account you use in
the server. Your flights are then posted in every channel chosen in each server you are in.
**Notification Channels** in settings lists them. There is no per-pilot opt-out yet: to stop
being posted in a server, leave it, or ask its admins to untick the channel.

ButterLog checks which servers you are in at most once an hour, when a flight of yours is
posted or when you open settings. A channel newly chosen by an admin is picked up straight
away. Joining or leaving a server can take up to an hour to show.

A flight begun in the air (a sim loaded mid-flight) is never posted.

## How it works

| Table | What it holds |
| --- | --- |
| `allowlisted_channels` | The channels admins chose: `channel_id`, `channel_name`, `guild_id`. |
| `discord_notification_channels` | Per pilot, the chosen channels of the servers they are in. A cache, rebuilt from the table above. |
| `users.discord_notification_channels_updated_at` | When that cache was rebuilt. Set to NULL for every pilot when a new channel is chosen. |
| `flight_discord_messages` | The message posted for each flight in each channel, so later syncs edit it rather than post again. |

- `discord::allowlist_channel`: called once the admin check and `validate_discord_channel`
  pass. It inserts the channel and clears every pilot's cache timestamp.
- `discord::maybe_update_user_notification_channels`: runs before each flight sync and on the
  settings page. If the cache is under an hour old it does nothing. Otherwise it asks Discord
  which of the allowlisted servers the pilot is a member of, and `store_notification_channels`
  rewrites the cache.
- `discord::is_user_admin_in_guild` and `validate_discord_channel` work out permissions the way
  Discord does: server roles first, then the channel's overwrites for @everyone, for roles,
  then for the member. Administrator overrides every overwrite. That logic is in
  `guild_permissions` and `channel_permissions`, and is unit tested.
- `discord::invite_url` builds the invite link from `DISCORD_CLIENT_ID` and
  `POSTING_PERMISSIONS`.

### Discord application settings

Under **Bot** in the [developer portal](https://discord.com/developers/applications):

- **Public Bot** must be on. Off, only the application's owner can add the bot, and anyone
  else gets an error from Discord's own page.
- **Requires OAuth2 Code Grant** must be off. The invite carries no redirect.
- Server Members intent is not needed. Member lookups go through the REST API.

## Testing

`cargo test` covers the permission rules and the invite link. With `TEST_DATABASE_URL` set, it
also covers which channels a pilot is posted in, and that choosing a new channel clears the
cache. Anything that talks to Discord itself can only be checked by hand.
Use a throwaway server, a second Discord account, and a ButterLog instance whose
`DISCORD_BOT_TOKEN` is a test application's.

1. Signed out, open `/content/discord/invite`. Discord's page names the application and the four
   permissions.
2. Add the bot to the test server as its owner. Open settings: the server is listed under
   Admin Controls with its text and announcement channels, but no voice channel or forum.
3. Make a channel private (deny View Channel to @everyone) and tick it. It is refused and the
   error names View Channel. Let the ButterLog role in, tick again: it is accepted.
4. Deny Attach Files to the ButterLog role in a channel and tick it. It is refused, naming
   Attach Files.
5. Sign in as the second account, a member with only Manage Server. The server is not under
   Admin Controls, and the page says Administrator is needed. Give it Administrator: the
   server appears.
6. As the second account, open settings. The ticked channel is under Notification Channels at
   once. Fly a short flight: it is posted there, edited in flight, and ends as a landing card
   with its screenshots.
7. Untick the channel. The second account's next flight is not posted there.
8. Tick it, delete it in Discord, and fly. The flight's post fails once, and the channel
   disappears from both admin and pilot settings.
