# Included first by boot.jl and by the background precompile (standalone.rs), before
# anything loads or checks the runtime's packages. Plain Julia: it must run before Pkg
# or EndeavorRuntime does.
#
# A compiled file of the runtime's own packages that this user can't read makes every
# later start fail: `Base.isprecompiled` and loading open it and throw "Permission
# denied", and Julia never compiles it again by itself (EndeavorMCP #372). Julia gives
# a new cache file its source file's mode plus write, so one compiled while its source
# was gone is left `--w-------`. Removing it lets Julia compile it again.
#
# A native image with no `.ji` beside it is never used, and Julia never removes it either: its
# own eviction works from the `.ji` files (#382). It is removed too, once it is old enough
# that no precompile can still be writing it.

# How old a native image with no `.ji` must be before it is removed: a precompile renames its
# files into place one at a time.
const ENDEAVOR_ORPHAN_AGE = 3600.0

const ENDEAVOR_OWN_PACKAGES = ("EndeavorRuntime", "EndeavorRuntimeWarm")
const ENDEAVOR_COMPILED_EXTS = (".ji", ".so", ".dylib", ".dll")

"""
Remove the compiled files of Endeavor's own packages in `depot` (the first entry of
DEPOT_PATH, ours, where Julia writes) that this user can't read, with the files of the
same name beside them (a `.ji` and its native image, and on macOS the image's `.dSYM`, go
together). Also remove a native image that has no `.ji` of its name and is older than
`ENDEAVOR_ORPHAN_AGE`, with its `.dSYM`. Only the EndeavorRuntime and EndeavorRuntimeWarm
folders, and never in the user's own depot (`~/.julia`). Temporary files (`jl_…`) a precompile
may be writing are left alone. The paths removed.
"""
function endeavor_drop_unreadable_caches(depot::AbstractString=first(DEPOT_PATH); readable=Sys.isreadable, log::IO=stderr, now::Float64=time())
    removed, dropped = String[], String[]
    compiled = joinpath(depot, "compiled")
    (isdir(compiled) && !samefile(depot, joinpath(homedir(), ".julia"))) || return removed
    versions = try readdir(compiled; join=true) catch; return removed end
    for version in versions, pkg in ENDEAVOR_OWN_PACKAGES
        dir = joinpath(version, pkg)
        isdir(dir) || continue
        names = try readdir(dir) catch; continue end
        compiled_file(name) = !startswith(name, "jl_") && splitext(name)[2] in ENDEAVOR_COMPILED_EXTS
        bad = Set(splitext(name)[1] for name in names if compiled_file(name) && isfile(joinpath(dir, name)) && !readable(joinpath(dir, name)))
        jis = Set(splitext(name)[1] for name in names if endswith(name, ".ji"))
        old(path) = try now - mtime(path) > ENDEAVOR_ORPHAN_AGE catch; false end
        image(name) = compiled_file(name) && !endswith(name, ".ji")
        orphans = Set(splitext(name)[1] for name in names if image(name) && !(splitext(name)[1] in jis) && !(splitext(name)[1] in bad) && old(joinpath(dir, name)))
        isempty(bad) && isempty(orphans) && continue
        for name in names
            # On macOS a native image may have a debug bundle beside it, `<stem>.dylib.dSYM`, which
            # Julia removes with it too.
            stem = compiled_file(name) ? splitext(name)[1] : endswith(name, ".dylib.dSYM") ? chop(name; tail=11) : nothing
            stem === nothing && continue
            stem in bad || stem in orphans || continue
            path = joinpath(dir, name)
            try
                rm(path; force=true, recursive=true)
                push!(stem in bad ? removed : dropped, path)
            catch e
                why = stem in bad ? "which can't be read, so Julia may fail to start until it's removed" : "which Julia doesn't use"
                println(log, "Couldn't remove $path, $why: ", sprint(showerror, e))
            end
        end
    end
    isempty(removed) || println(log, "Removed compiled files of Endeavor's runtime that couldn't be read; Julia compiles them again: ", join(removed, ", "))
    isempty(dropped) || println(log, "Removed compiled files of Endeavor's runtime that Julia no longer uses: ", join(dropped, ", "))
    append!(removed, dropped)
    flush(log)
    return removed
end
