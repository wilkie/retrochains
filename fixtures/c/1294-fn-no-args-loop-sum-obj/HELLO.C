int sum(void) {
  int s = 0;
  int i;
  for (i = 1; i <= 4; i++) s += i;
  return s;
}
int main(void) {
  return sum();
}
