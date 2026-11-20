# Included first by boot.jl and by the background precompile (standalone.rs), before
# anything loads or checks the runtime's packages. Plain Julia: it must run before Pkg
# or EndeavorRuntime does.
#
# A compiled file of the runtime's own packages that this user can't read makes every
# later start fail: `Base.isprecompiled` and loading open it and throw "Permission
# denied", and Julia never compiles it again by itself (EndeavorMCP #372). Julia gives
# a new cache file its source file's mode plus write, so one compiled while its source
# was gone is left `--w-------`. Removing it lets Julia compile it again.

const ENDEAVOR_OWN_PACKAGES = ("EndeavorRuntime", "EndeavorRuntimeWarm")
const ENDEAVOR_COMPILED_EXTS = (".ji", ".so", ".dylib", ".dll")

"""
Remove the compiled files of Endeavor's own packages in `depot` (the first entry of
DEPOT_PATH, ours, where Julia writes) that this user can't read, with the files of the
same name beside them (a `.ji` and its native image, and on macOS the image's `.dSYM`, go
together). Only the EndeavorRuntime and EndeavorRuntimeWarm folders, and never in the
user's own depot (`~/.julia`). Temporary files (`jl_…`) a precompile may be writing are left alone. The
paths removed.
"""
function endeavor_drop_unreadable_caches(depot::AbstractString=first(DEPOT_PATH); readable=Sys.isreadable, log::IO=stderr)
    removed = String[]
    compiled = joinpath(depot, "compiled")
    (isdir(compiled) && !samefile(depot, joinpath(homedir(), ".julia"))) || return removed
    versions = try readdir(compiled; join=true) catch; return removed end
    for version in versions, pkg in ENDEAVOR_OWN_PACKAGES
        dir = joinpath(version, pkg)
        isdir(dir) || continue
        names = try readdir(dir) catch; continue end
        compiled_file(name) = !startswith(name, "jl_") && splitext(name)[2] in ENDEAVOR_COMPILED_EXTS
        bad = Set(splitext(name)[1] for name in names if compiled_file(name) && isfile(joinpath(dir, name)) && !readable(joinpath(dir, name)))
        isempty(bad) && continue
        for name in names
            # On macOS a native image may have a debug bundle beside it, `<stem>.dylib.dSYM`, which
            # Julia removes with it too.
            ours = compiled_file(name) ? splitext(name)[1] in bad : endswith(name, ".dylib.dSYM") && name[1:end-11] in bad
            ours || continue
            path = joinpath(dir, name)
            try
                rm(path; force=true, recursive=true)
                push!(removed, path)
            catch e
                println(log, "Couldn't remove $path, which can't be read, so Julia may fail to start until it's removed: ", sprint(showerror, e))
            end
        end
    end
    isempty(removed) || println(log, "Removed compiled files of Endeavor's runtime that couldn't be read; Julia compiles them again: ", join(removed, ", "))
    flush(log)
    return removed
end
