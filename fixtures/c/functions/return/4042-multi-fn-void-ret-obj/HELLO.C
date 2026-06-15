int g = 0;
void set_g(int x) {
  g = x;
}
int main(void) {
  set_g(42);
  return g;
}
