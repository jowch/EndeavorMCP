# Pluto saves a notebook in place (`write(path, content)` in its `write_buffered`), so a process
# killed during the write leaves the `.jl` file cut short (#102). This method, more specific than
# Pluto's, is the one its `save_notebook(notebook, path::String)` calls: the content is written
# beside the file and renamed over it, so the file is either the old notebook or the new one. The
# file's permissions are kept, and a notebook opened through a symlink keeps its link: the file it
# points to is the one replaced. A file with hard links is written in place, so its names stay one file. Where that can't be done (a folder the notebook can't be written
# beside, a rename Windows refuses while another program has the file open), it is written in place
# as Pluto does.

const SAVES = Threads.Atomic{Int}(0)

# A staged file lives for as long as one write takes; one older than this was left by a process killed
# between the write and the rename (#469). Any process's, here or on another machine sharing the folder.
const STAGED_LEFT_AGE = 600.0

# The notebooks whose folder has been looked through for staged files left behind, once each per process.
const SWEPT = Set{String}()
const SWEPT_LOCK = ReentrantLock()

# Remove the staged copies of `target` that an earlier save left beside it (`.<name>.saving.<pid>.<n>`),
# once they are older than `STAGED_LEFT_AGE`. The notebook itself is whole either way.
function _remove_left_staged(target::String; now::Float64=time())
    prefix = "." * basename(target) * ".saving."
    dir = dirname(target)
    for name in readdir(isempty(dir) ? "." : dir)
        startswith(name, prefix) || continue
        staged = joinpath(dir, name)
        try
            isfile(staged) && now - mtime(staged) > STAGED_LEFT_AGE && rm(staged)
        catch
        end
    end
end

function Pluto.write_buffered(fn::Function, path::String)
    content = sprint(fn)
    target = islink(path) && isfile(path) ? realpath(path) : path
    # A rename would split a hard link from the file's other names.
    if isfile(target) && stat(target).nlink > 1
        write(target, content)
        return nothing
    end
    staged = joinpath(dirname(target), "." * basename(target) * ".saving." * string(getpid()) * "." * string(Threads.atomic_add!(SAVES, 1)))
    try
        write(staged, content)
        isfile(target) && chmod(staged, filemode(target) & 0o7777)
        Base.Filesystem.rename(staged, target)
        first = lock(SWEPT_LOCK) do
            target in SWEPT ? false : (push!(SWEPT, target); true)
        end
        first && try _remove_left_staged(target) catch end
    catch
        rm(staged; force=true)
        write(target, content)
    end
    nothing
end
