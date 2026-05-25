# The hashing function for Borland C++ 2.0 is a weakly hashed name in a bucket
# chained hash table. The hash table is 0x400 items large. The hash is the
# count (including the null terminator) multiplied by 64 added to the first
# 16-bit word in the name added to the final word in the name (not including the
# null terminator) multiplied by 8. It is then modulo to fit in the hash table
# size.
#
# This is an interesting hash function since collisions are generally common.
# Any function with the same first two characters alongside the same last two
# characters will match each other trivially. They are discovered and placed in
# the hash table in source order in the same linked list for that matching
# bucket.
def hash_name(name):
    count = len(name) + 1
    bytes = name.encode('utf-8')

    if count > 2:
        # Get the little endian word at the start of the name
        first_word = bytes[0] + (bytes[1] << 8)
        # Get the little endian word at the end of the name
        last_word = bytes[-2] + (bytes[-1] << 8)

        # compute hash
        return ((count << 6) + first_word + (last_word << 3)) & 0x3ff

    # Return the first byte if the name is short
    return bytes[0]

# Given a list of public externs...
decls = ['sum', 'main', 'b']

# Hash them and store them in our hash table
# The buckets are linked lists that are stored FIFO
hash_table = [None] * 0x400

for decl in decls:
    hash = hash_name(decl)

    if hash_table[hash] is None:
        # No list! Create one
        hash_table[hash] = []

    hash_table[hash].append(decl)

# Get listing by traversing the hash table in reverse (and the lists in reverse)
for item in reversed(hash_table):
    if item is not None:
        for name in reversed(item):
            print(f'  public {name}')
