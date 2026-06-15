struct B {
  unsigned int a : 6;
  unsigned int b : 6;
  unsigned int c : 4;
};
int main(void) {
  struct B s;
  s.a = 1;
  s.b = 2;
  s.c = 3;
  return (int)(s.a + s.b + s.c);
}
