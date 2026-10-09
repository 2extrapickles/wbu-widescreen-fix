Wakeboarding Unleashed Widescreen Fix
=====================================

Makes Wakeboarding Unleashed featuring Shaun Murray (PC, 2003) play properly on modern screens:
- any resolution (your desktop resolution by default)
- borderless window with instant alt-tab
- Hor+ field of view: wide screens see more at the sides
- widescreen HUD anchored to the screen edges (or centred, if you prefer)
- menus, loading screens and movies keep their original 4:3 layout (nothing stretched)
- 60 fps cap (configurable)
- fixes the "realloc relocated memory block!" crash when changing levels or quitting
Nothing on disk is changed; everything is patched in memory while the game runs.

INSTALL
  Copy d3d8.dll into the game's Game folder, next to WBU.EXE
  (default: C:\Program Files (x86)\Aspyr\Wakeboarding Unleashed\Game)
  and start the game as usual.

SETTINGS
  wbu-fix.ini (created on first run, or copy the included one next to d3d8.dll):
    width / height   0 = desktop resolution
    borderless       1 = borderless full screen, 0 = normal window
    fov_fix          1 = wider view on wide screens, 0 = original
    hud_fix          1 = HUD/menus/movies keep 4:3 proportions, 0 = stretched
    hud_layout       edges = HUD at the screen edges, center = HUD in the 4:3 area in the middle
    fps_limit        frames per second cap, 0 = uncapped

UNINSTALL
  Delete d3d8.dll, wbu-fix.ini and wbu-fix.log from the Game folder.

TROUBLESHOOTING
  wbu-fix.log next to WBU.EXE shows what was patched. If the game's exe is a different version
  than the fix was made for, nothing is patched and the log says so.
  To report a visual problem: set debug_keys=1, press F12 while it is on screen, quit, and send
  the wbu-shot-*.bmp file and wbu-fix.log.

Unofficial fan project; not affiliated with Activision, Aspyr or Shaba Games.
