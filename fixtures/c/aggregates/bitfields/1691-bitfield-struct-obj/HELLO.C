struct B {
  unsigned int a : 4;
  unsigned int b : 4;
  unsigned int c : 8;
};
int main(void) {
  struct B s;
  s.a = 3;
  s.b = 5;
  s.c = 100;
  return (int)(s.a + s.b + s.c);
}
