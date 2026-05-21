struct B { long a; };
int main(void) {
  struct B b = {100L};
  return (int)b.a;
}
