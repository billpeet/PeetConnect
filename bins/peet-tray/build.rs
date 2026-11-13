fn main() {
    // Resource IDs match `app::Icons`. FileDescription "PeetConnect" is the name Windows puts
    // on the tray's notifications.
    peet_winres::embed_with_icons(
        "PeetConnect",
        &[
            (2, "tray-active.ico"),
            (3, "tray-paused.ico"),
            (4, "tray-offline.ico"),
        ],
    );
}
