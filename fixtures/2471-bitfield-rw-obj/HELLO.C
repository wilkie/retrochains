struct Flags {
  unsigned int a : 3;
  unsigned int b : 4;
};
int main(void) {
  struct Flags f;
  f.a = 5;
  f.b = 10;
  return f.a + f.b;
}
