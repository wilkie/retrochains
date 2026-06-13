struct Flags {
  unsigned a : 4;
  unsigned b : 4;
} s;

void set_a(unsigned v) {
  s.a = v;
}
