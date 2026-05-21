int g = 42;
int main(void) {
  int near *p = &g;
  return *p;
}
