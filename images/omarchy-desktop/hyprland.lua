-- AHVM remote scanout; retain upstream Omarchy appearance and bindings.
hl.env("AQ_NO_KMS_REQUIREMENT", "1")
hl.monitor({ output = "Virtual-1", disabled = true })
hl.monitor({ output = "", mode = "1280x720@60", position = "auto", scale = 1 })
hl.config({ cursor = { no_hardware_cursors = true } })
hl.on("hyprland.start", function()
  hl.exec_cmd("/usr/local/bin/desktop-session")
end)
