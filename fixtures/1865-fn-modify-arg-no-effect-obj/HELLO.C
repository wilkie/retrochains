void mutate(int x) { x = 99; }
int main(void) {
  int x = 7;
  mutate(x);
  return x;
}
