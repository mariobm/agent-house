hl.env("AQ_NO_KMS_REQUIREMENT", "1")
hl.monitor({ output = "", mode = "1280x720@60", position = "auto", scale = 1 })
hl.monitor({ output = "Virtual-1", disabled = true })
hl.config({
    debug = { disable_logs = false, enable_stdout_logs = true },
    animations = { enabled = false },
    misc = { disable_hyprland_logo = true, disable_splash_rendering = true },
    cursor = { no_hardware_cursors = true },
})
hl.on("hyprland.start", function ()
    hl.exec_cmd("/usr/local/bin/desktop-session")
end)
