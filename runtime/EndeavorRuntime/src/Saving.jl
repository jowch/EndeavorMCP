# Pluto saves a notebook in place (`write(path, content)` in its `write_buffered`), so a process
# killed during the write leaves the `.jl` file cut short (#102). This method, more specific than
# Pluto's, is the one its `save_notebook(notebook, path::String)` calls: the content is written
# beside the file and renamed over it, so the file is either the old notebook or the new one. The
# file's permissions are kept, and a notebook opened through a symlink keeps its link: the file it
# points to is the one replaced. Where that can't be done (a folder the notebook can't be written
# beside, a rename Windows refuses while another program has the file open), it is written in place
# as Pluto does.

const SAVES = Threads.Atomic{Int}(0)

function Pluto.write_buffered(fn::Function, path::String)
    content = sprint(fn)
    target = islink(path) && isfile(path) ? realpath(path) : path
    staged = joinpath(dirname(target), "." * basename(target) * ".saving." * string(getpid()) * "." * string(Threads.atomic_add!(SAVES, 1)))
    try
        write(staged, content)
        isfile(target) && chmod(staged, filemode(target) & 0o7777)
        Base.Filesystem.rename(staged, target)
    catch
        rm(staged; force=true)
        write(target, content)
    end
    nothing
end
