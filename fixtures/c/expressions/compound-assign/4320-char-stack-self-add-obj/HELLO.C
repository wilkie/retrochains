int sink(void) {
  return 0;
}
int main(void) {
  char c = 5;
  c = c + 1;
  sink();
  return c;
}
