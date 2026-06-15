int g;
int *getp(void) {
  return &g;
}
int main(void) {
  *getp() = 7;
  return g;
}
