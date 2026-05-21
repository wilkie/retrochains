int zero(void) { return 0; }
int one(void) { return 1; }
int neg_one(void) { return -1; }
int main(void) {
  return zero() + one() * neg_one();
}
