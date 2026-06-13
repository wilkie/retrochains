int two(void) { return 2; }
int three(void) { return 3; }
struct Op { int (*fn)(void); int id; };
int main(void) {
  static struct Op ops[2] = {{two, 1}, {three, 2}};
  return ops[0].fn() + ops[1].fn() + ops[0].id + ops[1].id;
}
