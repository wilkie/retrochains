int next_a(void) { static int counter = 0; return ++counter; }
int next_b(void) { static int counter = 100; return ++counter; }
int main(void) {
  return next_a() + next_b() + next_a() + next_b();
}
