from substrate import Environment

with Environment.create(workspace="new", allow_commands=["ls"]) as env:
    env.write("notes.txt", "hello")
    print(env.read("notes.txt"))
    print(env.bash("ls /workspace"))
