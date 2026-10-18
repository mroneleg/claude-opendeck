# OpenAction catalogue submission

Checklist for listing this plugin in the
[OpenAction plugin catalogue](https://github.com/OpenActionAPI/plugins)
(what its `validate_submission.py` checks):

1. **Repository settings** (GitHub → About):
   - Description (must match the catalogue entry exactly):
     `OpenDeck plugin: Claude plan usage, pace and alerts, plus Claude Code session status`
   - Topic: `openaction`
2. **A release** whose assets include `com.mroneleg.claudeusage.streamDeckPlugin`
   (push a `v<version>` tag; `.github/workflows/release.yml` builds and publishes it).
3. **Catalogue entry**, added to the "Native OpenAction plugins" section of
   `catalogue.json`, sorted by repository URL (tab-indented, as `json.dumps(..., indent="\t")`):

   ```json
   	"com.mroneleg.claudeusage": {
   		"name": "Claude Usage",
   		"author": "Mr. One Leg",
   		"repository": "https://github.com/mroneleg/claude-opendeck",
   		"description": "OpenDeck plugin: Claude plan usage, pace and alerts, plus Claude Code session status",
   		"flags": {
   			"ai_assisted": true
   		}
   	},
   ```

   `name` and `author` match `assets/manifest.json`; the key is the bundle ID
   that every action UUID starts with.
4. **Icon**: copy `assets/icons/plugin@2x.png` (144×144, the manifest's plugin
   icon) to `icons/com.mroneleg.claudeusage.png` in the catalogue repo.
