zbus-xmlgen file ../../data/interfaces/com.steampowered.SteamOSManager1.xml
zbus-xmlgen file ../../data/interfaces/com.steampowered.SteamOSManager1.Manager.xml

for f in *.rs; do
    if [[ $f == "lib.rs" ]]; then
        continue
    fi

    if [[ $f == "job1.rs" || $f == "job_manager1.rs" ]]; then
        patch $f -p0 < patches/$f.patch
        continue
    fi

    patch $f -p0 < patches/fix.patch
done
