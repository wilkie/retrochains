struct Flags {
  unsigned a : 3;
  unsigned b : 5;
};
struct Flags f;
int main(void) {
  f.a = 5;
  f.b = 17;
  return f.b;
}
