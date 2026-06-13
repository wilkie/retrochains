int classify(int x) {
  if (x < 0) return -1;
  if (x == 0) return 0;
  if (x < 10) return 1;
  return 2;
}
int main(void) {
  return classify(5);
}
