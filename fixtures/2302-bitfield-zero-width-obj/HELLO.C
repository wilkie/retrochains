struct Z { unsigned int a : 3; unsigned int : 0; unsigned int b : 3; };
int main(void) {
  struct Z z;
  z.a = 5;
  z.b = 6;
  return (int)(z.a + z.b) + (int)sizeof(struct Z);
}
